CREATE TABLE workspace (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    created_at INTEGER NOT NULL CHECK (created_at >= 0)
);

INSERT INTO workspace(singleton, name, created_at)
VALUES (1, 'MDTree Workspace', 0);

CREATE TABLE nodes (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    slug TEXT NOT NULL CHECK (length(slug) > 0),
    summary TEXT,
    node_type TEXT,
    markdown_content TEXT NOT NULL DEFAULT '',
    sibling_order INTEGER NOT NULL DEFAULT 0 CHECK (sibling_order >= 0),
    content_version INTEGER NOT NULL DEFAULT 1 CHECK (content_version >= 1),
    content_hash BLOB NOT NULL CHECK (length(content_hash) = 32),
    revision_hash BLOB NOT NULL CHECK (length(revision_hash) = 32),
    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    FOREIGN KEY(parent_id) REFERENCES nodes(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    UNIQUE(parent_id, slug)
);

CREATE UNIQUE INDEX nodes_single_root
ON nodes((1))
WHERE parent_id IS NULL;

CREATE INDEX nodes_parent_order
ON nodes(parent_id, sibling_order, id);
