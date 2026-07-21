ALTER TABLE workspace ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;

CREATE TRIGGER workspace_revision_on_node_insert
AFTER INSERT ON nodes
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER workspace_revision_on_node_update
AFTER UPDATE ON nodes
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER workspace_revision_on_node_delete
AFTER DELETE ON nodes
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER workspace_revision_on_reference_insert
AFTER INSERT ON "references"
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER workspace_revision_on_reference_update
AFTER UPDATE ON "references"
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;

CREATE TRIGGER workspace_revision_on_reference_delete
AFTER DELETE ON "references"
BEGIN
    UPDATE workspace SET revision = revision + 1 WHERE singleton = 1;
END;
