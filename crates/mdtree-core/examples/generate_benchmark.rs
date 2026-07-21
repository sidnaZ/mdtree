//! Writes a deterministic benchmark JSON snapshot.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let destination = std::env::args_os()
        .nth(1)
        .ok_or("destination path required")?;
    let count = std::env::args()
        .nth(2)
        .map_or(Ok(10_000), |value| value.parse())?;
    let seed = std::env::args()
        .nth(3)
        .map_or(Ok(42), |value| value.parse())?;
    let mut bytes =
        serde_json::to_vec_pretty(&mdtree_core::generate_benchmark_snapshot(count, seed))?;
    bytes.push(b'\n');
    std::fs::write(destination, bytes)?;
    Ok(())
}
