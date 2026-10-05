-- Image assets referenced from Markdown as `asset:<name>`. Bytes are stored
-- once per distinct content (`asset_blobs`); names map onto them.
CREATE TABLE asset_blobs (
    hash BLOB PRIMARY KEY CHECK (length(hash) = 32),
    data BLOB NOT NULL
);

CREATE TABLE assets (
    name TEXT PRIMARY KEY CHECK (length(name) BETWEEN 1 AND 128),
    hash BLOB NOT NULL REFERENCES asset_blobs(hash) ON UPDATE RESTRICT ON DELETE RESTRICT,
    media_type TEXT NOT NULL CHECK (media_type IN ('image/png', 'image/jpeg', 'image/gif')),
    byte_size INTEGER NOT NULL CHECK (byte_size >= 0),
    width INTEGER NOT NULL CHECK (width > 0),
    height INTEGER NOT NULL CHECK (height > 0),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at)
);

CREATE INDEX assets_hash ON assets(hash);

CREATE TRIGGER workspace_revision_on_asset_insert
AFTER INSERT ON assets
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER workspace_revision_on_asset_update
AFTER UPDATE ON assets
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER workspace_revision_on_asset_delete
AFTER DELETE ON assets
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;
