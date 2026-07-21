//! Emits reproducible executable/service resource measurements as JSON.

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count = std::env::args()
        .nth(1)
        .map_or(Ok(10_000), |value| value.parse())?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("measurement.mdtree");
    let snapshot = mdtree_core::generate_benchmark_snapshot(count, 42);
    let import_start = Instant::now();
    mdtree_sqlite::import_snapshot_new(&path, &snapshot)?;
    let import_micros = import_start.elapsed().as_micros();
    let open_start = Instant::now();
    let store = mdtree_sqlite::SqliteStore::open(&path)?;
    let open_micros = open_start.elapsed().as_micros();
    let root = store.root()?.id();
    let context = store.read_context(root, 65_536)?;
    let context_bytes = serde_json::to_vec(&context)?.len();
    let workspace_status_bytes =
        serde_json::to_vec(&mdtree_sqlite::workspace_status(store.connection(), &path)?)?.len();
    let max_rss_kib = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
        });
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "node_count": count,
            "seed": 42,
            "import_micros": import_micros,
            "open_micros": open_micros,
            "database_bytes": std::fs::metadata(&path)?.len(),
            "max_rss_kib_linux": max_rss_kib,
            "read_context_bytes": context_bytes,
            "workspace_status_resource_bytes": workspace_status_bytes
        }))?
    );
    Ok(())
}
