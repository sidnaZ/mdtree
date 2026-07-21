//! Ordered, embedded, transactional database migrations.

use rusqlite::{params, Connection};
use thiserror::Error;

/// Latest schema migration understood by this executable.
pub const LATEST_SCHEMA_VERSION: u32 = 6;
/// Canonical workspace data format created by this executable.
pub const WORKSPACE_FORMAT_VERSION: u32 = 1;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "workspace_format",
        sql: include_str!("../migrations/0001_workspace_format.sql"),
    },
    Migration {
        version: 2,
        name: "canonical_nodes",
        sql: include_str!("../migrations/0002_canonical_nodes.sql"),
    },
    Migration {
        version: 3,
        name: "derived_and_history",
        sql: include_str!("../migrations/0003_derived_and_history.sql"),
    },
    Migration {
        version: 4,
        name: "section_fts",
        sql: include_str!("../migrations/0004_section_fts.sql"),
    },
    Migration {
        version: 5,
        name: "mutation_receipts",
        sql: include_str!("../migrations/0005_mutation_receipts.sql"),
    },
    Migration {
        version: 6,
        name: "workspace_revision",
        sql: include_str!("../migrations/0006_workspace_revision.sql"),
    },
];

#[derive(Clone, Copy)]
struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

/// Failure while inspecting or applying schema migrations.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// The database reports a schema newer than this executable supports.
    #[error("database schema version {found} is newer than supported version {supported}")]
    NewerSchema {
        /// Version found in the database.
        found: u32,
        /// Latest version supported by this executable.
        supported: u32,
    },
    /// A specific ordered migration failed and the migration transaction was rolled back.
    #[error("migration {version} ({name}) failed: {source}")]
    Apply {
        /// Ordered migration version.
        version: u32,
        /// Stable migration name.
        name: &'static str,
        /// Underlying `SQLite` error.
        #[source]
        source: rusqlite::Error,
    },
    /// General `SQLite` failure while managing the migration transaction.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Applies every pending embedded migration in one transaction.
///
/// # Errors
///
/// Returns [`MigrationError`] when schema inspection, a migration statement,
/// or the final commit fails. Any schema changes made during this call are
/// rolled back together.
pub fn migrate(connection: &mut Connection) -> Result<(), MigrationError> {
    connection.pragma_update(None, "foreign_keys", true)?;
    apply_migrations(connection, MIGRATIONS)
}

fn apply_migrations(
    connection: &mut Connection,
    migrations: &[Migration],
) -> Result<(), MigrationError> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY CHECK (version > 0),
            name TEXT NOT NULL UNIQUE
        );",
    )?;

    let current_i64: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    let current = u32::try_from(current_i64).map_err(|_| MigrationError::NewerSchema {
        found: u32::MAX,
        supported: LATEST_SCHEMA_VERSION,
    })?;

    let latest = migrations.last().map_or(0, |migration| migration.version);
    if current > latest {
        return Err(MigrationError::NewerSchema {
            found: current,
            supported: latest,
        });
    }

    for migration in migrations
        .iter()
        .filter(|migration| migration.version > current)
    {
        transaction
            .execute_batch(migration.sql)
            .map_err(|source| MigrationError::Apply {
                version: migration.version,
                name: migration.name,
                source,
            })?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name) VALUES (?1, ?2)",
            params![migration.version, migration.name],
        )?;
    }

    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::{params, Connection};

    use super::{apply_migrations, migrate, Migration, LATEST_SCHEMA_VERSION};

    #[test]
    fn new_database_migrates_to_latest_version() {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        migrate(&mut connection).expect("migrations should succeed");

        let version: u32 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("schema version");
        assert_eq!(version, LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn failed_migration_rolls_back_the_complete_batch() {
        let migrations = [
            Migration {
                version: 1,
                name: "valid",
                sql: "CREATE TABLE should_rollback(id INTEGER PRIMARY KEY);",
            },
            Migration {
                version: 2,
                name: "invalid",
                sql: "CREATE TABLE broken(",
            },
        ];
        let mut connection = Connection::open_in_memory().expect("in-memory database");

        assert!(apply_migrations(&mut connection, &migrations).is_err());
        let table_count: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('schema_migrations', 'should_rollback')",
                [],
                |row| row.get(0),
            )
            .expect("table count");
        assert_eq!(table_count, 0);
    }

    #[test]
    fn canonical_schema_contains_exactly_one_workspace_record() {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        migrate(&mut connection).expect("migrations should succeed");

        let count: u32 = connection
            .query_row("SELECT COUNT(*) FROM workspace", [], |row| row.get(0))
            .expect("workspace count");
        assert_eq!(count, 1);
        assert!(connection
            .execute(
                "INSERT INTO workspace(singleton, name, created_at) VALUES (2, 'Other', 0)",
                [],
            )
            .is_err());
    }

    #[test]
    fn canonical_schema_rejects_orphans() {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        migrate(&mut connection).expect("migrations should succeed");

        assert!(insert_node(&connection, "child", Some("missing"), "child").is_err());
    }

    #[test]
    fn canonical_schema_rejects_duplicate_sibling_slugs() {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        migrate(&mut connection).expect("migrations should succeed");
        insert_node(&connection, "root", None, "project").expect("root insert");
        insert_node(&connection, "child-a", Some("root"), "database").expect("first child");

        assert!(insert_node(&connection, "child-b", Some("root"), "database").is_err());
    }

    #[test]
    fn derived_schema_enforces_foreign_keys_and_unique_versions() {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        migrate(&mut connection).expect("migrations should succeed");
        insert_node(&connection, "root", None, "project").expect("root insert");

        assert!(connection
            .execute(
                "INSERT INTO sections (
                    id, node_id, start_byte, end_byte, content, content_hash, position
                 ) VALUES ('orphan-section', 'missing', 0, 0, '', ?1, 0)",
                [vec![3_u8; 32]],
            )
            .is_err());

        let insert_revision = |version| {
            connection.execute(
                "INSERT INTO node_versions (
                    node_id, version, title, slug, markdown_content, sibling_order,
                    metadata_json, content_hash, revision_hash, created_at
                 ) VALUES ('root', ?1, 'Root', 'project', '', 0, '{}', ?2, ?3, 1)",
                params![version, vec![1_u8; 32], vec![2_u8; 32]],
            )
        };
        insert_revision(1).expect("first revision");
        assert!(insert_revision(1).is_err());
    }

    #[test]
    fn fts_schema_supports_section_smoke_query() {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        migrate(&mut connection).expect("FTS5 migration should succeed");
        connection
            .execute(
                "INSERT INTO section_fts(section_id, node_id, title, heading, content)
                 VALUES ('section', 'node', 'Database Models', 'Products', 'Inventory schema')",
                [],
            )
            .expect("FTS row");

        let result: String = connection
            .query_row(
                "SELECT section_id FROM section_fts WHERE section_fts MATCH 'inventory'",
                [],
                |row| row.get(0),
            )
            .expect("FTS match");
        assert_eq!(result, "section");
    }

    fn insert_node(
        connection: &Connection,
        id: &str,
        parent_id: Option<&str>,
        slug: &str,
    ) -> rusqlite::Result<usize> {
        connection.execute(
            "INSERT INTO nodes (
                id, parent_id, title, slug, content_hash, revision_hash, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 1)",
            params![id, parent_id, id, slug, vec![1_u8; 32], vec![2_u8; 32]],
        )
    }
}
