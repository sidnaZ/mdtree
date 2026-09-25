//! Real-process coverage for focused and heterogeneous mutation batches.

use std::path::Path;
use std::process::Command;

use tempfile::tempdir;

fn run_json(workspace: &Path, arguments: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_mdtree"))
        .arg("--workspace")
        .arg(workspace)
        .arg("--output")
        .arg("json")
        .args(arguments)
        .output()
        .expect("run mdtree");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

#[test]
fn cli_batches_commit_once_and_resolve_temporary_create_labels() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("batch.mdtree");
    run_json(&workspace, &["init", "Project"]);
    let left = run_json(&workspace, &["create", "project", "Left"])["node_id"]
        .as_str()
        .expect("left id")
        .to_owned();
    let destination = run_json(&workspace, &["create", "project", "Destination"])["node_id"]
        .as_str()
        .expect("destination id")
        .to_owned();
    let removed = run_json(&workspace, &["create", "project", "Removed"])["node_id"]
        .as_str()
        .expect("removed id")
        .to_owned();

    let focused_path = directory.path().join("focused.json");
    std::fs::write(
        &focused_path,
        serde_json::to_vec(&serde_json::json!({
            "moves": [{"selector": left, "destination_parent": destination, "expected_version": 1}],
            "removals": [{"selector": removed, "expected_version": 1}]
        }))
        .expect("focused JSON"),
    )
    .expect("write focused request");
    let focused = run_json(
        &workspace,
        &["atomic-tree-batch", focused_path.to_str().expect("path")],
    );
    assert_eq!(focused["status"], "applied");
    assert_eq!(focused["removed_node_count"], 1);
    assert_eq!(
        run_json(&workspace, &["show", &left])[0]["parent_id"],
        destination
    );

    let generic_path = directory.path().join("generic.json");
    std::fs::write(&generic_path, serde_json::to_vec(&serde_json::json!({
        "operations": [
            {"kind":"create", "label":"new-parent", "parent":"project", "title":"New Parent"},
            {"kind":"create", "label":"new-child", "parent":"new-parent", "title":"New Child"},
            {"kind":"update", "selector":left, "content":"# Left\n\nChanged in batch.\n", "expected_version":2}
        ]
    })).expect("generic JSON")).expect("write generic request");
    let generic = run_json(
        &workspace,
        &["mutation-batch", generic_path.to_str().expect("path")],
    );
    assert_eq!(generic["status"], "applied");
    assert_eq!(generic["operation_count"], 3);
    assert!(
        run_json(&workspace, &["show", &left])[0]["markdown_content"]
            .as_str()
            .expect("content")
            .contains("Changed in batch")
    );
    let child = run_json(&workspace, &["show", "new-child"]);
    assert_eq!(child[0]["metadata"]["title"], "New Child");
}

#[test]
fn cli_rename_preserves_the_slug_by_default_and_regenerates_on_request() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("rename.mdtree");
    run_json(&workspace, &["init", "Project"]);
    let leaf = run_json(&workspace, &["create", "project", "Leaf"])["node_id"]
        .as_str()
        .expect("leaf id")
        .to_owned();
    let slug_and_title = |workspace: &Path| {
        let node = &run_json(workspace, &["show", &leaf])[0];
        (
            node["slug"].as_str().expect("slug").to_owned(),
            node["metadata"]["title"]
                .as_str()
                .expect("title")
                .to_owned(),
        )
    };

    run_json(
        &workspace,
        &["rename", &leaf, "Leaf Renamed", "--expected-version", "1"],
    );
    assert_eq!(
        slug_and_title(&workspace),
        ("leaf".into(), "Leaf Renamed".into())
    );

    run_json(
        &workspace,
        &[
            "rename",
            &leaf,
            "Brand New",
            "--slug",
            "regenerate",
            "--expected-version",
            "2",
        ],
    );
    assert_eq!(
        slug_and_title(&workspace),
        ("brand-new".into(), "Brand New".into())
    );

    // The node's own current slug never counts as a collision.
    run_json(
        &workspace,
        &[
            "rename",
            &leaf,
            "BRAND NEW",
            "--slug",
            "regenerate",
            "--expected-version",
            "3",
        ],
    );
    assert_eq!(
        slug_and_title(&workspace),
        ("brand-new".into(), "BRAND NEW".into())
    );

    let batch_path = directory.path().join("rename-batch.json");
    std::fs::write(
        &batch_path,
        serde_json::to_vec(&serde_json::json!({
            "operations": [
                {"kind":"rename", "selector":leaf, "title":"Batch Renamed", "expected_version":4}
            ]
        }))
        .expect("batch JSON"),
    )
    .expect("write batch request");
    run_json(
        &workspace,
        &["mutation-batch", batch_path.to_str().expect("path")],
    );
    assert_eq!(
        slug_and_title(&workspace),
        ("brand-new".into(), "Batch Renamed".into())
    );
}
