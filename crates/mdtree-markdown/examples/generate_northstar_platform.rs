//! Regenerates the checked-in Northstar Platform interchange fixtures.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let snapshot = mdtree_core::northstar_platform_snapshot();
    let mut json = serde_json::to_vec_pretty(&snapshot)?;
    json.push(b'\n');
    std::fs::write(root.join("examples/northstar-platform.snapshot.json"), json)?;
    mdtree_markdown::export_markdown_snapshot(
        &root.join("examples/northstar-platform-markdown"),
        &snapshot,
    )?;
    Ok(())
}
