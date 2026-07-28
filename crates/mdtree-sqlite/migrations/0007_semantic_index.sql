CREATE TABLE semantic_profiles (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0),
    dimensions INTEGER NOT NULL CHECK (dimensions > 0),
    metric TEXT NOT NULL CHECK (metric IN ('cosine')),
    input_format_version INTEGER NOT NULL CHECK (input_format_version > 0),
    UNIQUE(provider, model, dimensions, metric, input_format_version)
);

CREATE TABLE semantic_index (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    active_profile_id INTEGER,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    FOREIGN KEY(active_profile_id) REFERENCES semantic_profiles(id)
);

INSERT INTO semantic_index(singleton, active_profile_id, revision)
VALUES (1, NULL, 0);

CREATE TABLE semantic_chunks (
    id INTEGER PRIMARY KEY,
    profile_id INTEGER NOT NULL,
    node_id TEXT NOT NULL,
    section_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    start_byte INTEGER NOT NULL CHECK (start_byte >= 0),
    end_byte INTEGER NOT NULL CHECK (end_byte >= start_byte),
    input TEXT NOT NULL,
    input_hash BLOB NOT NULL CHECK (length(input_hash) = 32),
    state TEXT NOT NULL CHECK (state IN ('pending', 'processing', 'ready', 'failed')),
    embedding BLOB,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error TEXT,
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
    FOREIGN KEY(profile_id) REFERENCES semantic_profiles(id) ON DELETE CASCADE,
    FOREIGN KEY(node_id) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY(section_id) REFERENCES sections(id) ON DELETE CASCADE,
    UNIQUE(profile_id, section_id, position),
    CHECK (
        (state = 'ready' AND embedding IS NOT NULL AND last_error IS NULL)
        OR (state = 'failed' AND embedding IS NULL AND length(last_error) > 0)
        OR (state IN ('pending', 'processing') AND embedding IS NULL AND last_error IS NULL)
    )
);

CREATE INDEX semantic_chunks_profile_state
ON semantic_chunks(profile_id, state, id);

CREATE INDEX semantic_chunks_profile_input_hash
ON semantic_chunks(profile_id, input_hash)
WHERE state = 'ready';

CREATE INDEX semantic_chunks_node_profile
ON semantic_chunks(node_id, profile_id);

CREATE TRIGGER semantic_revision_on_chunk_insert
AFTER INSERT ON semantic_chunks
BEGIN
    UPDATE semantic_index SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER semantic_revision_on_chunk_update
AFTER UPDATE ON semantic_chunks
BEGIN
    UPDATE semantic_index SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER semantic_revision_on_chunk_delete
AFTER DELETE ON semantic_chunks
BEGIN
    UPDATE semantic_index SET revision = revision + 1 WHERE singleton = 1;
END;
