//! Real-process coverage for bounded structural collection enumeration.

use std::path::Path;
use std::process::Command;

use tempfile::tempdir;

fn mdtree() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mdtree"))
}

fn run_json(workspace: &Path, arguments: &[&str]) -> serde_json::Value {
    let output = mdtree()
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

fn fixture() -> mdtree_core::LargeTreeFixture {
    mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 120,
            deep_descendants: 24,
            history_revisions: 4,
            relations: 16,
            response_boundary_bytes: 4096,
        },
        105,
    )
}

#[test]
fn cli_children_and_siblings_pages_enumerate_wide_sets_without_gaps() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("wide.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");

    for (command, selector) in [
        ("children", fixture.wide_parent_id),
        ("siblings", fixture.wide_child_ids[63]),
    ] {
        let selector = selector.to_string();
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let mut arguments = vec![command, selector.as_str(), "--limit", "37"];
            if let Some(value) = cursor.as_deref() {
                arguments.extend(["--cursor", value]);
            }
            let page = run_json(&workspace, &arguments);
            ids.extend(
                page["items"]
                    .as_array()
                    .expect("page items")
                    .iter()
                    .map(|node| node["id"].as_str().expect("node ID").to_owned()),
            );
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            assert_eq!(page["complete"], cursor.is_none());
            assert_eq!(page["truncated"], cursor.is_some());
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            ids,
            fixture
                .wide_child_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            "{command}"
        );
    }
}

#[test]
fn cli_descendant_and_subtree_pages_equal_complete_dfs_with_depths() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("dfs.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let store = mdtree_sqlite::SqliteStore::open(&workspace).expect("store");
    let full_subtree = store
        .subtree(fixture.root_id)
        .expect("complete subtree")
        .into_iter()
        .map(|row| (row.depth, row.node.id().to_string()))
        .collect::<Vec<_>>();

    for (command, expected) in [
        ("subtree", full_subtree.as_slice()),
        ("descendants", &full_subtree[1..]),
    ] {
        let selector = fixture.root_id.to_string();
        let mut cursor = None;
        let mut rows = Vec::new();
        loop {
            let mut arguments = vec![command, selector.as_str(), "--limit", "11"];
            if let Some(value) = cursor.as_deref() {
                arguments.extend(["--cursor", value]);
            }
            let page = run_json(&workspace, &arguments);
            rows.extend(
                page["items"]
                    .as_array()
                    .expect("page items")
                    .iter()
                    .map(|row| {
                        (
                            u32::try_from(row["depth"].as_u64().expect("depth"))
                                .expect("u32 depth"),
                            row["node"]["id"].as_str().expect("node ID").to_owned(),
                        )
                    }),
            );
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(rows, expected, "{command}");
    }

    let mut expected_bfs = full_subtree.clone();
    expected_bfs.sort_by_key(|(depth, _)| *depth);
    let selector = fixture.root_id.to_string();
    let mut cursor = None;
    let mut rows = Vec::new();
    loop {
        let mut arguments = vec![
            "subtree",
            selector.as_str(),
            "--order",
            "bfs",
            "--limit",
            "11",
        ];
        if let Some(value) = cursor.as_deref() {
            arguments.extend(["--cursor", value]);
        }
        let page = run_json(&workspace, &arguments);
        rows.extend(
            page["items"]
                .as_array()
                .expect("BFS items")
                .iter()
                .map(|row| {
                    (
                        u32::try_from(row["depth"].as_u64().expect("depth")).expect("u32 depth"),
                        row["node"]["id"].as_str().expect("node ID").to_owned(),
                    )
                }),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(rows, expected_bfs);
}

#[test]
fn cli_inspect_pages_reach_items_after_one_hundred_without_repeating_root() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("inspect.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let selector = fixture.wide_parent_id.to_string();
    let mut cursor = None;
    let mut rows = Vec::new();
    loop {
        let mut arguments = vec![
            "inspect",
            selector.as_str(),
            "--depth",
            "1",
            "--limit",
            "37",
        ];
        if let Some(value) = cursor.as_deref() {
            arguments.extend(["--cursor", value]);
        }
        let page = run_json(&workspace, &arguments);
        rows.extend(
            page["items"]
                .as_array()
                .expect("inspection items")
                .iter()
                .cloned(),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        assert_eq!(page["truncated"], cursor.is_some());
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(rows.len(), fixture.wide_child_ids.len() + 1);
    assert_eq!(rows[0]["node"]["node_id"], selector);
    assert_eq!(rows[0]["depth"], 0);
    assert_eq!(rows[0]["child_count"], 120);
    for (row, expected_id) in rows[1..].iter().zip(&fixture.wide_child_ids) {
        assert_eq!(row["node"]["node_id"], expected_id.to_string());
        assert_eq!(row["depth"], 1);
        assert_eq!(row["child_count"], 0);
    }
}

#[test]
fn cli_rejects_invalid_limits_and_stale_continuations() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("errors.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");

    let invalid = mdtree()
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "children",
            &fixture.wide_parent_id.to_string(),
            "--limit",
            "0",
        ])
        .output()
        .expect("invalid limit");
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("outside 1..=100"));

    let first = run_json(
        &workspace,
        &[
            "children",
            &fixture.wide_parent_id.to_string(),
            "--limit",
            "2",
        ],
    );
    let cursor = first["next_cursor"].as_str().expect("cursor");
    let store = mdtree_sqlite::SqliteStore::open(&workspace).expect("store");
    store
        .connection()
        .execute(
            "UPDATE nodes SET updated_at=updated_at+1 WHERE id=?1",
            [fixture.wide_child_ids[0].to_string()],
        )
        .expect("canonical mutation");
    let stale = mdtree()
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "children",
            &fixture.wide_parent_id.to_string(),
            "--limit",
            "2",
            "--cursor",
            cursor,
        ])
        .output()
        .expect("stale continuation");
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale pagination cursor"));
}

#[test]
fn cli_search_pages_enumerate_stable_ties_for_every_scope() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("search.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let store = mdtree_sqlite::SqliteStore::open(&workspace).expect("store");
    for child in &fixture.wide_child_ids {
        store
            .connection()
            .execute(
                "INSERT OR IGNORE INTO \"references\" (source_node_id,target_node_id,target_ref,reference_type,origin) VALUES (?1,?2,?2,'cli_search_scope','explicit')",
                [fixture.wide_parent_id.to_string(), child.to_string()],
            )
            .expect("reference");
    }
    drop(store);

    let mut expected = fixture
        .wide_child_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    expected.sort_unstable();
    for (scope, scope_node) in [
        ("workspace", None),
        ("subtree", Some(fixture.wide_parent_id)),
        ("siblings", Some(fixture.wide_child_ids[0])),
        ("parent_subtree", Some(fixture.wide_child_ids[0])),
        ("linked", Some(fixture.wide_parent_id)),
    ] {
        let scope_node = scope_node.map(|id| id.to_string());
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let mut arguments = vec![
                "search",
                "deterministic wide fixture",
                "--scope",
                scope,
                "--limit",
                "37",
            ];
            if let Some(value) = scope_node.as_deref() {
                arguments.extend(["--scope-node", value]);
            }
            if let Some(value) = cursor.as_deref() {
                arguments.extend(["--cursor", value]);
            }
            let page = run_json(&workspace, &arguments);
            ids.extend(
                page["items"]
                    .as_array()
                    .expect("search items")
                    .iter()
                    .map(|item| item["node_id"].as_str().expect("node ID").to_owned()),
            );
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(ids, expected, "{scope}");
    }

    let one = run_json(
        &workspace,
        &[
            "search",
            "deterministic wide fixture",
            "--scope",
            "current_node",
            "--scope-node",
            &fixture.wide_child_ids[0].to_string(),
        ],
    );
    assert_eq!(one["items"].as_array().expect("items").len(), 1);

    let first = run_json(
        &workspace,
        &["search", "deterministic wide fixture", "--limit", "2"],
    );
    let cursor = first["next_cursor"].as_str().expect("cursor");
    let changed = mdtree()
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "search",
            "different query",
            "--limit",
            "2",
            "--cursor",
            cursor,
        ])
        .output()
        .expect("changed query continuation");
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("does not match this request"));
}

#[test]
fn cli_reference_pages_enumerate_resolved_unresolved_and_backlinks() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("reference-pages.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 175,
            deep_descendants: 0,
            history_revisions: 1,
            relations: 0,
            response_boundary_bytes: 256,
        },
        913,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let store = mdtree_sqlite::SqliteStore::open(&workspace).expect("store");
    for (index, child) in fixture.wide_child_ids.iter().enumerate() {
        store
            .connection()
            .execute(
                "INSERT INTO \"references\" (source_node_id,target_node_id,target_ref,reference_type,origin,metadata_json) VALUES (?1,?2,?3,?4,'explicit',?5)",
                (
                    fixture.root_id.to_string(),
                    Some(child.to_string()),
                    child.to_string(),
                    "outgoing_page",
                    format!(r#"{{"fixture_index":{index}}}"#),
                ),
            )
            .expect("outgoing reference");
        store
            .connection()
            .execute(
                "INSERT INTO \"references\" (source_node_id,target_node_id,target_ref,reference_type,origin,metadata_json) VALUES (?1,?2,?3,?4,'explicit',?5)",
                (
                    child.to_string(),
                    Some(fixture.root_id.to_string()),
                    fixture.root_id.to_string(),
                    "backlink_page",
                    format!(r#"{{"fixture_index":{index}}}"#),
                ),
            )
            .expect("backlink");
    }
    store
        .connection()
        .execute(
            "UPDATE \"references\" SET target_node_id=NULL,target_ref='missing-target' WHERE source_node_id=?1 AND reference_type='outgoing_page' AND json_extract(metadata_json,'$.fixture_index')=174",
            [fixture.root_id.to_string()],
        )
        .expect("unresolved reference");
    drop(store);

    for (command, selector, relation) in [
        ("references", fixture.root_id, "outgoing_page"),
        ("backlinks", fixture.root_id, "backlink_page"),
    ] {
        let selector = selector.to_string();
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let mut arguments = vec![command, selector.as_str(), "--limit", "37"];
            if let Some(value) = cursor.as_deref() {
                arguments.extend(["--cursor", value]);
            }
            let page = run_json(&workspace, &arguments);
            items.extend(
                page["items"]
                    .as_array()
                    .expect("reference items")
                    .iter()
                    .cloned(),
            );
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            assert_eq!(page["complete"], cursor.is_none());
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(items.len(), 175, "{command}");
        assert!(items.iter().all(|item| item["reference_type"] == relation));
        assert_eq!(
            items
                .iter()
                .map(|item| item["metadata"]["fixture_index"].as_u64().expect("index"))
                .collect::<Vec<_>>(),
            (0..175).collect::<Vec<_>>()
        );
        if command == "references" {
            assert_eq!(items[174]["target"]["status"], "unresolved");
            assert_eq!(items[174]["target"]["target_ref"], "missing-target");
        }
    }
}

#[test]
fn cli_history_pages_enumerate_long_history_newest_first() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("history-pages.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 0,
            deep_descendants: 0,
            history_revisions: 250,
            relations: 0,
            response_boundary_bytes: 128,
        },
        914,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let selector = fixture.history_node_id.to_string();
    let mut cursor = None;
    let mut versions = Vec::new();
    loop {
        let mut arguments = vec!["history", selector.as_str(), "--limit", "100"];
        if let Some(value) = cursor.as_deref() {
            arguments.extend(["--cursor", value]);
        }
        let page = run_json(&workspace, &arguments);
        versions.extend(
            page["items"]
                .as_array()
                .expect("history items")
                .iter()
                .map(|item| item["version"].as_u64().expect("version")),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(versions, (1..=250).rev().collect::<Vec<_>>());
}

#[test]
fn cli_revision_returns_exact_immutable_state_without_changing_head() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("exact-revision.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 0,
            deep_descendants: 0,
            history_revisions: 7,
            relations: 0,
            response_boundary_bytes: 128,
        },
        915,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let selector = fixture.history_node_id.to_string();
    let revision = run_json(&workspace, &["revision", &selector, "3"]);
    assert_eq!(revision["version"], 3);
    assert_eq!(revision["node_id"], selector);
    assert!(revision["markdown_content"]
        .as_str()
        .expect("content")
        .contains("revision 00003"));
    let head = run_json(&workspace, &["show", &selector]);
    assert_eq!(head[0]["version"], 7);
}

#[test]
fn cli_validate_pages_corrupt_findings_without_repair() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("validate-pages.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 120,
            deep_descendants: 0,
            history_revisions: 1,
            relations: 0,
            response_boundary_bytes: 128,
        },
        916,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let store = mdtree_sqlite::SqliteStore::open(&workspace).expect("store");
    store
        .connection()
        .execute(
            "UPDATE nodes SET content_hash=zeroblob(32) WHERE parent_id=?1",
            [fixture.wide_parent_id.to_string()],
        )
        .expect("inject corruption");
    drop(store);
    let mut cursor = None;
    let mut count = 0;
    loop {
        let mut command = mdtree();
        command
            .arg("--workspace")
            .arg(&workspace)
            .args(["--output", "json", "validate", "--limit", "37"]);
        if let Some(value) = cursor.as_deref() {
            command.args(["--cursor", value]);
        }
        let output = command.output().expect("validate process");
        assert_eq!(output.status.code(), Some(2));
        let page: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
        count += page["items"].as_array().expect("findings").len();
        assert_eq!(page["total_findings"], 120);
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(count, 120);
}

#[test]
fn cli_statistics_returns_database_side_aggregates() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("statistics.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let result = run_json(&workspace, &["statistics", &fixture.root_id.to_string()]);
    assert_eq!(result["direct_child_count"], 5);
    assert_eq!(result["subtree_size"], 150);
    assert_eq!(result["leaf_count"], 124);
    assert_eq!(result["max_relative_depth"], 25);
    assert_eq!(result["max_width"], 121);
}

#[test]
fn cli_contains_and_lca_cover_same_and_distant_nodes() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("ancestry.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let contains = run_json(
        &workspace,
        &[
            "contains",
            &fixture.root_id.to_string(),
            &fixture.deep_leaf_id.to_string(),
        ],
    );
    assert_eq!(contains["contains"], true);
    let lca = run_json(
        &workspace,
        &[
            "lowest-common-ancestor",
            &fixture.wide_child_ids[0].to_string(),
            &fixture.wide_child_ids[119].to_string(),
        ],
    );
    assert_eq!(lca["id"], fixture.wide_parent_id.to_string());
    let path = run_json(
        &workspace,
        &[
            "path-between",
            &fixture.wide_child_ids[0].to_string(),
            &fixture.deep_leaf_id.to_string(),
        ],
    );
    assert_eq!(path["distance"], 27);
    assert_eq!(
        path["nodes"][0]["id"],
        fixture.wide_child_ids[0].to_string()
    );
    assert_eq!(path["nodes"][2]["id"], fixture.root_id.to_string());
    assert_eq!(path["nodes"][27]["id"], fixture.deep_leaf_id.to_string());
    let distance = run_json(
        &workspace,
        &[
            "distance",
            &fixture.wide_child_ids[0].to_string(),
            &fixture.deep_leaf_id.to_string(),
        ],
    );
    assert_eq!(distance["distance"], 27);
}

#[test]
fn cli_filter_pages_apply_leaf_predicate_before_pagination() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("filter.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let selector = fixture.root_id.to_string();
    let mut cursor = None;
    let mut items = Vec::new();
    loop {
        let mut arguments = vec![
            "filter",
            selector.as_str(),
            "--predicate",
            "leaf",
            "--limit",
            "37",
        ];
        if let Some(value) = cursor.as_deref() {
            arguments.extend(["--cursor", value]);
        }
        let page = run_json(&workspace, &arguments);
        items.extend(page["items"].as_array().expect("items").iter().cloned());
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(items.len(), 124);
    let bounded = run_json(
        &workspace,
        &[
            "filter",
            &selector,
            "--predicate",
            "internal",
            "--max-depth",
            "1",
        ],
    );
    assert_eq!(
        bounded["items"].as_array().expect("internal items").len(),
        3
    );
}

#[test]
fn cli_indexed_child_and_adjacent_siblings_cover_boundaries() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("indexed-navigation.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");

    let child = run_json(
        &workspace,
        &["child-at", &fixture.wide_parent_id.to_string(), "63"],
    );
    assert_eq!(child["index"], 63);
    assert_eq!(child["node"]["id"], fixture.wide_child_ids[63].to_string());
    let missing = run_json(
        &workspace,
        &["child-at", &fixture.wide_parent_id.to_string(), "120"],
    );
    assert!(missing["node"].is_null());

    let neighbors = run_json(
        &workspace,
        &["adjacent-siblings", &fixture.wide_child_ids[63].to_string()],
    );
    assert_eq!(
        neighbors["previous"]["id"],
        fixture.wide_child_ids[62].to_string()
    );
    assert_eq!(
        neighbors["next"]["id"],
        fixture.wide_child_ids[64].to_string()
    );
    let root = run_json(
        &workspace,
        &["adjacent-siblings", &fixture.root_id.to_string()],
    );
    assert!(root["previous"].is_null());
    assert!(root["next"].is_null());
}

#[test]
fn cli_search_exposes_node_type_and_required_tag_filters() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("search-filters.mdtree");
    mdtree_sqlite::import_snapshot_new(&workspace, &mdtree_core::developer_workspace_snapshot())
        .expect("fixture import");
    let page = run_json(
        &workspace,
        &[
            "search",
            "benchmark",
            "--node-type",
            "work_item",
            "--tag",
            "benchmark",
            "--status",
            "has_status",
            "--min-depth",
            "2",
            "--max-depth",
            "2",
            "--created-from",
            "1750000000000",
            "--updated-to",
            "1750000000000",
            "--structure",
            "internal",
            "--limit",
            "1",
        ],
    );
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "T091 — 10,000-Node Benchmark Generator");
}

#[test]
fn cli_batch_node_lookup_preserves_order_duplicates_and_errors() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("batch-node.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let root = fixture.root_id.to_string();
    let result = run_json(
        &workspace,
        &["batch-node", &root, "wide-00000", &root, "missing-node"],
    );
    let items = result.as_array().expect("batch items");
    assert_eq!(items.len(), 4);
    assert_eq!(items[0]["node"]["id"], root);
    assert_eq!(
        items[1]["node"]["id"],
        fixture.wide_child_ids[0].to_string()
    );
    assert_eq!(items[2]["node"]["id"], fixture.root_id.to_string());
    assert_eq!(items[3]["error"]["code"], "not_found");

    let wide = fixture.wide_parent_id.to_string();
    let grouped = run_json(
        &workspace,
        &["batch-children", &wide, &root, "--limit", "37"],
    );
    let groups = grouped.as_array().expect("groups");
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups[0]["page"]["items"].as_array().expect("wide").len(),
        37
    );
    let cursor = groups[0]["page"]["next_cursor"].as_str().expect("cursor");
    let cursor_arg = format!("{wide}={cursor}");
    let continued = run_json(
        &workspace,
        &[
            "batch-children",
            &wide,
            "--limit",
            "37",
            "--cursor",
            &cursor_arg,
        ],
    );
    assert_eq!(
        continued[0]["page"]["items"]
            .as_array()
            .expect("continued")
            .len(),
        37
    );
}

#[test]
fn cli_clone_subtree_plans_then_applies_atomically() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("clone-subtree.mdtree");
    let fixture = fixture();
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let source = fixture.deep_parent_id.to_string();
    let destination = fixture.root_id.to_string();
    let planned = run_json(
        &workspace,
        &[
            "clone-subtree",
            &source,
            &destination,
            "--expected-version",
            "1",
            "--dry-run",
        ],
    );
    assert_eq!(planned["status"], "planned");
    assert_eq!(planned["node_count"], 25);
    let applied = run_json(
        &workspace,
        &[
            "clone-subtree",
            &source,
            &destination,
            "--expected-version",
            "1",
        ],
    );
    assert_eq!(applied["status"], "applied");
    let clone_id = applied["cloned_root_id"].as_str().expect("clone ID");
    let shown = run_json(&workspace, &["statistics", clone_id]);
    assert_eq!(shown["subtree_size"], 25);
}
