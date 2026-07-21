CREATE TABLE mutation_receipts (
    operation_id TEXT PRIMARY KEY CHECK (length(trim(operation_id)) > 0),
    tool_name TEXT NOT NULL CHECK (length(trim(tool_name)) > 0),
    payload_hash BLOB NOT NULL CHECK (length(payload_hash) = 32),
    result_json TEXT NOT NULL CHECK (json_valid(result_json)),
    created_at INTEGER NOT NULL CHECK (created_at >= 0)
) STRICT;
