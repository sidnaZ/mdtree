CREATE TABLE workspace_format (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    format_version INTEGER NOT NULL CHECK (format_version > 0)
);

INSERT INTO workspace_format(singleton, format_version) VALUES (1, 1);
