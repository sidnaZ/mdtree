//! Consistent `SQLite` connection setup.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

/// Opens and configures a file-backed workspace connection.
///
/// # Errors
///
/// Returns a `rusqlite` error if the database cannot be opened or a required
/// pragma cannot be applied.
pub fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    configure(&connection, true)?;
    Ok(connection)
}

/// Opens and configures an isolated in-memory connection.
///
/// # Errors
///
/// Returns a `rusqlite` error if a required pragma cannot be applied.
pub fn open_memory_connection() -> rusqlite::Result<Connection> {
    let connection = Connection::open_in_memory()?;
    configure(&connection, false)?;
    Ok(connection)
}

fn configure(connection: &Connection, file_backed: bool) -> rusqlite::Result<()> {
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    if file_backed {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use tempfile::NamedTempFile;

    use super::{open_connection, open_memory_connection};
    use crate::migrate;

    #[test]
    fn every_opened_connection_enables_foreign_keys() {
        let connection = open_memory_connection().expect("configured connection");
        let enabled: bool = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign key pragma");
        assert!(enabled);
    }

    #[test]
    fn file_workspace_supports_concurrent_readers() {
        let file = NamedTempFile::new().expect("temporary workspace");
        let path = file.path().to_path_buf();
        let mut writer = open_connection(&path).expect("writer connection");
        migrate(&mut writer).expect("workspace schema");

        let barrier = Arc::new(Barrier::new(3));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let connection = open_connection(&path).expect("reader connection");
                    barrier.wait();
                    connection
                        .query_row("SELECT COUNT(*) FROM workspace", [], |row| {
                            row.get::<_, u32>(0)
                        })
                        .expect("concurrent read")
                })
            })
            .collect();
        barrier.wait();

        for reader in readers {
            assert_eq!(reader.join().expect("reader thread"), 1);
        }
    }
}
