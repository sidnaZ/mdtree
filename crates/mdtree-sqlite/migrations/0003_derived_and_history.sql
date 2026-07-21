CREATE TABLE sections (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    parent_section_id TEXT,
    heading TEXT,
    heading_level INTEGER CHECK (heading_level BETWEEN 1 AND 6),
    anchor TEXT,
    start_byte INTEGER NOT NULL CHECK (start_byte >= 0),
    end_byte INTEGER NOT NULL CHECK (end_byte >= start_byte),
    content TEXT NOT NULL,
    content_hash BLOB NOT NULL CHECK (length(content_hash) = 32),
    position INTEGER NOT NULL CHECK (position >= 0),
    FOREIGN KEY(node_id) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY(parent_section_id) REFERENCES sections(id) ON DELETE CASCADE,
    UNIQUE(node_id, position),
    UNIQUE(node_id, anchor)
);

CREATE TABLE node_versions (
    id INTEGER PRIMARY KEY,
    node_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    parent_id TEXT,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    slug TEXT NOT NULL,
    markdown_content TEXT NOT NULL,
    sibling_order INTEGER NOT NULL CHECK (sibling_order >= 0),
    metadata_json TEXT NOT NULL CHECK (json_valid(metadata_json)),
    content_hash BLOB NOT NULL CHECK (length(content_hash) = 32),
    revision_hash BLOB NOT NULL CHECK (length(revision_hash) = 32),
    change_summary TEXT,
    created_by TEXT,
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    FOREIGN KEY(node_id) REFERENCES nodes(id) ON DELETE CASCADE,
    UNIQUE(node_id, version)
);

CREATE TABLE "references" (
    id INTEGER PRIMARY KEY,
    source_node_id TEXT NOT NULL,
    target_node_id TEXT,
    target_ref TEXT,
    target_anchor TEXT,
    reference_type TEXT NOT NULL CHECK (length(trim(reference_type)) > 0),
    source_section_id TEXT,
    origin TEXT NOT NULL CHECK (origin IN (
        'explicit', 'markdown', 'wikilink', 'imported_metadata', 'inferred', 'agent'
    )),
    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
    FOREIGN KEY(source_node_id) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY(target_node_id) REFERENCES nodes(id) ON DELETE SET NULL,
    FOREIGN KEY(source_section_id) REFERENCES sections(id) ON DELETE CASCADE,
    CHECK (target_node_id IS NOT NULL OR length(target_ref) > 0)
);

CREATE INDEX sections_node_position ON sections(node_id, position);
CREATE INDEX node_versions_node_version ON node_versions(node_id, version DESC);
CREATE INDEX references_source ON "references"(source_node_id, reference_type);
CREATE INDEX references_target ON "references"(target_node_id, reference_type);
CREATE INDEX references_unresolved ON "references"(target_ref) WHERE target_node_id IS NULL;
