//! Regenerates the portable developer workspace under `examples/`.

use std::path::{Path, PathBuf};

use mdtree_core::{developer_workspace_snapshot, Snapshot};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let custom_output = std::env::args_os().nth(1).map(PathBuf::from);
    let output = custom_output
        .clone()
        .unwrap_or_else(|| repository.join("examples"));
    std::fs::create_dir_all(&output)?;
    generate(
        &output.join("developer-workspace.mdtree"),
        &developer_workspace_snapshot(),
    )?;
    println!("generated developer workspace in {}", output.display());
    Ok(())
}

fn generate(path: &Path, snapshot: &Snapshot) -> Result<(), Box<dyn std::error::Error>> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    mdtree_sqlite::import_snapshot_new(path, snapshot)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}
