//! Real-process stdio protocol coverage against the canonical Northstar Platform.

use std::fmt::Write as _;

use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tempfile::tempdir;

fn assert_canonical_nodes(value: &serde_json::Value, expected: &[mdtree_core::NodeId]) {
    let nodes = value.as_array().expect("node array");
    assert_eq!(nodes.len(), expected.len());
    for (node, expected_id) in nodes.iter().zip(expected) {
        assert_eq!(node["id"], serde_json::json!(expected_id));
    }
    assert!(nodes.windows(2).all(|pair| {
        let left = (
            pair[0]["sibling_order"].as_u64().expect("left order"),
            pair[0]["id"].as_str().expect("left id"),
        );
        let right = (
            pair[1]["sibling_order"].as_u64().expect("right order"),
            pair[1]["id"].as_str().expect("right id"),
        );
        left < right
    }));
    assert!(nodes.windows(2).any(|pair| {
        pair[0]["id"].as_str().expect("left id") > pair[1]["id"].as_str().expect("right id")
    }));
}

fn tool_json(result: rmcp::model::CallToolResult) -> serde_json::Value {
    let encoded = serde_json::to_value(result).expect("tool result JSON");
    serde_json::from_str(
        encoded["content"][0]["text"]
            .as_str()
            .expect("tool JSON text"),
    )
    .expect("tool JSON")
}

fn deep_fixture_ids(fixture: &mdtree_core::LargeTreeFixture) -> Vec<mdtree_core::NodeId> {
    fixture
        .snapshot
        .nodes
        .iter()
        .filter(|node| node.id == fixture.deep_parent_id || node.slug.as_str().starts_with("deep-"))
        .map(|node| node.id)
        .collect()
}

async fn root_result(current_directory: &std::path::Path, fallback: &std::path::Path) -> String {
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command
                .current_dir(current_directory)
                .arg("--fallback-workspace")
                .arg(fallback);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let result = client
        .peer()
        .call_tool(CallToolRequestParams::new("root"))
        .await
        .expect("root tool");
    client.cancel().await.expect("shutdown");
    serde_json::to_string(&result).expect("root result JSON")
}

#[tokio::test]
async fn real_stdio_history_pages_enumerate_long_history_newest_first() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("history-pages.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 0,
            deep_descendants: 0,
            history_revisions: 250,
            relations: 0,
            response_boundary_bytes: 128,
        },
        122,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let mut cursor: Option<String> = None;
    let mut versions = Vec::new();
    loop {
        let mut arguments = serde_json::json!({
            "selector": fixture.history_node_id,
            "limit": 100
        });
        if let Some(value) = cursor.as_ref() {
            arguments["cursor"] = serde_json::Value::String(value.clone());
        }
        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("history")
                    .with_arguments(arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect("history page");
        let page = tool_json(result);
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

    let exact = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("revision").with_arguments(
                serde_json::json!({"selector":fixture.history_node_id,"version":3})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("exact revision");
    let exact = tool_json(exact);
    assert_eq!(exact["version"], 3);
    assert!(exact["markdown_content"]
        .as_str()
        .expect("content")
        .contains("revision 00003"));
    let head = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("node").with_arguments(
                serde_json::json!({"selector":fixture.history_node_id})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("current head");
    assert_eq!(tool_json(head)[0]["version"], 250);
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_validate_pages_corrupt_findings_without_repair() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("validate-pages.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 120,
            deep_descendants: 0,
            history_revisions: 1,
            relations: 0,
            response_boundary_bytes: 128,
        },
        123,
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
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let mut cursor: Option<String> = None;
    let mut count = 0;
    loop {
        let mut arguments = serde_json::json!({"limit":37});
        if let Some(value) = cursor.as_ref() {
            arguments["cursor"] = serde_json::Value::String(value.clone());
        }
        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("validate")
                    .with_arguments(arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect("validate page");
        let page = tool_json(result);
        assert_eq!(page["healthy"], false);
        assert_eq!(page["total_findings"], 120);
        count += page["items"].as_array().expect("findings").len();
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(count, 120);
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_exposes_version_and_bounded_subtree_diffs() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("diffs.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 120,
            deep_descendants: 8,
            history_revisions: 6,
            relations: 8,
            response_boundary_bytes: 4096,
        },
        121,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("scale import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");

    let version = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("version_diff").with_arguments(
                serde_json::json!({
                    "selector":fixture.history_node_id,
                    "from":1,
                    "to":6
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("version diff");
    assert_eq!(tool_json(version)["changed"], true);

    let first = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("subtree_diff").with_arguments(
                serde_json::json!({
                    "from_selector":fixture.root_id,
                    "to_selector":fixture.wide_parent_id,
                    "limit":10
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("subtree diff");
    let first = tool_json(first);
    assert_eq!(first["items"].as_array().expect("items").len(), 10);
    assert_eq!(first["truncated"], true);
    let cursor = first["next_cursor"].as_str().expect("cursor");
    let second = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("subtree_diff").with_arguments(
                serde_json::json!({
                    "from_selector":fixture.root_id,
                    "to_selector":fixture.wide_parent_id,
                    "limit":10,
                    "cursor":cursor
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("continued subtree diff");
    assert_eq!(
        tool_json(second)["items"].as_array().expect("items").len(),
        10
    );
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn real_stdio_reuses_composite_scale_fixture_for_wide_and_bounded_reads() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("scale.mdtree");
    let spec = mdtree_core::LargeTreeFixtureSpec {
        wide_children: 120,
        deep_descendants: 24,
        history_revisions: 12,
        relations: 48,
        response_boundary_bytes: 4096,
    };
    let fixture = mdtree_core::generate_large_tree_fixture(spec, 91);
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("scale import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");

    let children = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("children").with_arguments(
                serde_json::json!({"selector":fixture.wide_parent_id,"limit":100})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("wide children");
    let statistics = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("statistics").with_arguments(
                serde_json::json!({"selector":fixture.root_id})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("statistics");
    let statistics = tool_json(statistics);
    assert_eq!(statistics["direct_child_count"], 5);
    assert_eq!(statistics["subtree_size"], 150);
    assert_eq!(statistics["leaf_count"], 124);
    assert_eq!(statistics["max_relative_depth"], 25);
    assert_eq!(statistics["max_width"], 121);
    let contains = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("contains").with_arguments(
                serde_json::json!({
                    "ancestor":fixture.root_id,
                    "descendant":fixture.deep_leaf_id
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("contains");
    assert_eq!(tool_json(contains)["contains"], true);
    let lca = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("lowest_common_ancestor").with_arguments(
                serde_json::json!({
                    "left":fixture.wide_child_ids[0],
                    "right":fixture.wide_child_ids[119]
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("LCA");
    assert_eq!(tool_json(lca)["id"], fixture.wide_parent_id.to_string());
    let path = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("path_between").with_arguments(
                serde_json::json!({
                    "left":fixture.wide_child_ids[0],
                    "right":fixture.deep_leaf_id
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("path between");
    let path = tool_json(path);
    assert_eq!(path["distance"], 27);
    assert_eq!(
        path["nodes"][0]["id"],
        fixture.wide_child_ids[0].to_string()
    );
    assert_eq!(path["nodes"][27]["id"], fixture.deep_leaf_id.to_string());
    let distance = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("distance").with_arguments(
                serde_json::json!({
                    "left":fixture.wide_child_ids[0],
                    "right":fixture.deep_leaf_id
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("distance");
    assert_eq!(tool_json(distance)["distance"], 27);
    let mut cursor: Option<String> = None;
    let mut leaf_count = 0;
    loop {
        let mut arguments = serde_json::json!({
            "selector":fixture.root_id,
            "predicate":"leaf",
            "limit":37
        });
        if let Some(value) = cursor.as_ref() {
            arguments["cursor"] = serde_json::Value::String(value.clone());
        }
        let page = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("filter_nodes")
                    .with_arguments(arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect("leaf page");
        let page = tool_json(page);
        leaf_count += page["items"].as_array().expect("items").len();
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(leaf_count, 124);
    let mut bfs_cursor: Option<String> = None;
    let mut bfs_depths = Vec::new();
    loop {
        let mut arguments = serde_json::json!({
            "selector":fixture.root_id,
            "order":"bfs",
            "limit":17
        });
        if let Some(value) = bfs_cursor.as_ref() {
            arguments["cursor"] = serde_json::Value::String(value.clone());
        }
        let page = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("subtree")
                    .with_arguments(arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect("BFS page");
        let page = tool_json(page);
        bfs_depths.extend(
            page["items"]
                .as_array()
                .expect("BFS items")
                .iter()
                .map(|item| item["depth"].as_u64().expect("depth")),
        );
        bfs_cursor = page["next_cursor"].as_str().map(str::to_owned);
        if bfs_cursor.is_none() {
            break;
        }
    }
    assert_eq!(bfs_depths.len(), 150);
    assert!(bfs_depths.windows(2).all(|pair| pair[0] <= pair[1]));
    let batch = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("batch_nodes").with_arguments(
                serde_json::json!({"selectors":[
                    fixture.root_id.to_string(),
                    "wide-00000",
                    fixture.root_id.to_string(),
                    "missing-node"
                ]})
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("batch nodes");
    let batch = tool_json(batch);
    assert_eq!(batch.as_array().expect("batch items").len(), 4);
    assert_eq!(batch[0]["node"]["id"], fixture.root_id.to_string());
    assert_eq!(batch[2]["node"]["id"], fixture.root_id.to_string());
    assert_eq!(batch[3]["error"]["code"], "not_found");
    let child_groups = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("batch_children").with_arguments(
                serde_json::json!({"requests":[
                    {"parent":fixture.wide_parent_id,"limit":37},
                    {"parent":fixture.root_id,"limit":5},
                    {"parent":"missing-node","limit":1}
                ]})
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("batch children");
    let child_groups = tool_json(child_groups);
    assert_eq!(child_groups.as_array().expect("groups").len(), 3);
    assert_eq!(
        child_groups[0]["page"]["items"]
            .as_array()
            .expect("wide page")
            .len(),
        37
    );
    assert_eq!(child_groups[2]["error"]["code"], "not_found");
    let child = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("child_at").with_arguments(
                serde_json::json!({
                    "parent":fixture.wide_parent_id,
                    "index":63
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("indexed child");
    assert_eq!(
        tool_json(child)["node"]["id"],
        fixture.wide_child_ids[63].to_string()
    );
    let neighbors = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("adjacent_siblings").with_arguments(
                serde_json::json!({"selector":fixture.wide_child_ids[63]})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("adjacent siblings");
    let neighbors = tool_json(neighbors);
    assert_eq!(
        neighbors["previous"]["id"],
        fixture.wide_child_ids[62].to_string()
    );
    assert_eq!(
        neighbors["next"]["id"],
        fixture.wide_child_ids[64].to_string()
    );
    let children_json = serde_json::to_value(children).expect("children result");
    let children_value: serde_json::Value = serde_json::from_str(
        children_json["content"][0]["text"]
            .as_str()
            .expect("children JSON text"),
    )
    .expect("children JSON");
    assert_eq!(
        children_value["items"].as_array().expect("items").len(),
        100
    );
    assert_eq!(children_value["complete"], false);
    assert_eq!(children_value["truncated"], true);
    let children_cursor = children_value["next_cursor"]
        .as_str()
        .expect("children cursor");
    let continued_children = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("children").with_arguments(
                serde_json::json!({
                    "selector":fixture.wide_parent_id,
                    "limit":100,
                    "cursor":children_cursor
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("continued children");
    let continued_json = serde_json::to_value(continued_children).expect("children result");
    let continued_value: serde_json::Value = serde_json::from_str(
        continued_json["content"][0]["text"]
            .as_str()
            .expect("children JSON text"),
    )
    .expect("children JSON");
    assert_eq!(continued_value["complete"], true);
    assert_eq!(continued_value["truncated"], false);
    assert!(continued_value["next_cursor"].is_null());
    let mut all_children = children_value["items"].as_array().expect("items").clone();
    all_children.extend(
        continued_value["items"]
            .as_array()
            .expect("continued items")
            .iter()
            .cloned(),
    );
    assert_canonical_nodes(
        &serde_json::Value::Array(all_children),
        &fixture.wide_child_ids,
    );

    let siblings = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("siblings").with_arguments(
                serde_json::json!({"selector":fixture.wide_child_ids[73],"limit":100})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("wide siblings");
    let siblings_json = serde_json::to_value(siblings).expect("siblings result");
    let siblings_value: serde_json::Value = serde_json::from_str(
        siblings_json["content"][0]["text"]
            .as_str()
            .expect("siblings JSON text"),
    )
    .expect("siblings JSON");
    assert_eq!(
        siblings_value["items"].as_array().expect("items").len(),
        100
    );
    let siblings_cursor = siblings_value["next_cursor"]
        .as_str()
        .expect("siblings cursor");
    let continued_siblings = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("siblings").with_arguments(
                serde_json::json!({
                    "selector":fixture.wide_child_ids[73],
                    "limit":100,
                    "cursor":siblings_cursor
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("continued siblings");
    let continued_json = serde_json::to_value(continued_siblings).expect("siblings result");
    let continued_value: serde_json::Value = serde_json::from_str(
        continued_json["content"][0]["text"]
            .as_str()
            .expect("siblings JSON text"),
    )
    .expect("siblings JSON");
    let mut all_siblings = siblings_value["items"].as_array().expect("items").clone();
    all_siblings.extend(
        continued_value["items"]
            .as_array()
            .expect("continued items")
            .iter()
            .cloned(),
    );
    assert_canonical_nodes(
        &serde_json::Value::Array(all_siblings),
        &fixture.wide_child_ids,
    );

    let deep_ids = deep_fixture_ids(&fixture);
    let ancestors = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("ancestors").with_arguments(
                serde_json::json!({"selector":fixture.deep_leaf_id})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("deep ancestors");
    let ancestors_json = serde_json::to_value(ancestors).expect("ancestors result");
    let ancestors_value: serde_json::Value = serde_json::from_str(
        ancestors_json["content"][0]["text"]
            .as_str()
            .expect("ancestors JSON text"),
    )
    .expect("ancestors JSON");
    let ancestor_ids = std::iter::once(fixture.root_id)
        .chain(deep_ids[..deep_ids.len() - 1].iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(
        ancestors_value.as_array().expect("ancestors").len(),
        ancestor_ids.len()
    );
    for (row, expected_id) in ancestors_value
        .as_array()
        .expect("ancestors")
        .iter()
        .zip(ancestor_ids)
    {
        assert_eq!(row["node"]["id"], serde_json::json!(expected_id));
    }

    let mut subtree_rows = Vec::new();
    let mut subtree_cursor: Option<String> = None;
    loop {
        let subtree = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("subtree").with_arguments(
                    serde_json::json!({
                        "selector":fixture.deep_parent_id,
                        "limit":10,
                        "cursor":subtree_cursor
                    })
                    .as_object()
                    .expect("arguments")
                    .clone(),
                ),
            )
            .await
            .expect("deep subtree");
        let subtree_json = serde_json::to_value(subtree).expect("subtree result");
        let subtree_value: serde_json::Value = serde_json::from_str(
            subtree_json["content"][0]["text"]
                .as_str()
                .expect("subtree JSON text"),
        )
        .expect("subtree JSON");
        subtree_rows.extend(
            subtree_value["items"]
                .as_array()
                .expect("subtree items")
                .iter()
                .cloned(),
        );
        subtree_cursor = subtree_value["next_cursor"].as_str().map(str::to_owned);
        if subtree_cursor.is_none() {
            assert_eq!(subtree_value["complete"], true);
            break;
        }
    }
    assert_eq!(subtree_rows.len(), deep_ids.len());
    for (depth, (row, expected_id)) in subtree_rows.iter().zip(&deep_ids).enumerate() {
        assert_eq!(row["depth"], depth);
        assert_eq!(row["node"]["id"], serde_json::json!(expected_id));
    }

    let mut descendant_rows = Vec::new();
    let mut descendant_cursor: Option<String> = None;
    loop {
        let descendants = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("descendants").with_arguments(
                    serde_json::json!({
                        "selector":fixture.deep_parent_id,
                        "limit":10,
                        "cursor":descendant_cursor
                    })
                    .as_object()
                    .expect("arguments")
                    .clone(),
                ),
            )
            .await
            .expect("deep descendants");
        let result_json = serde_json::to_value(descendants).expect("descendant result");
        let page: serde_json::Value = serde_json::from_str(
            result_json["content"][0]["text"]
                .as_str()
                .expect("descendant JSON text"),
        )
        .expect("descendant JSON");
        descendant_rows.extend(
            page["items"]
                .as_array()
                .expect("descendant items")
                .iter()
                .cloned(),
        );
        descendant_cursor = page["next_cursor"].as_str().map(str::to_owned);
        if descendant_cursor.is_none() {
            break;
        }
    }
    assert_eq!(descendant_rows.len(), deep_ids.len() - 1);
    for (depth, (row, expected_id)) in descendant_rows.iter().zip(&deep_ids[1..]).enumerate() {
        assert_eq!(row["depth"], depth + 1);
        assert_eq!(row["node"]["id"], serde_json::json!(expected_id));
    }

    for tool in ["inspect", "navigate"] {
        let mut cursor: Option<String> = None;
        let mut inspection_rows = Vec::new();
        loop {
            let result = client
                .peer()
                .call_tool(
                    CallToolRequestParams::new(tool).with_arguments(
                        serde_json::json!({
                            "selector":fixture.wide_parent_id,
                            "depth":1,
                            "limit":37,
                            "cursor":cursor
                        })
                        .as_object()
                        .expect("arguments")
                        .clone(),
                    ),
                )
                .await
                .expect("bounded inspection page");
            let result_json = serde_json::to_value(result).expect("inspection result");
            let page: serde_json::Value = serde_json::from_str(
                result_json["content"][0]["text"]
                    .as_str()
                    .expect("inspection JSON text"),
            )
            .expect("inspection JSON");
            inspection_rows.extend(
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
        assert_eq!(inspection_rows.len(), fixture.wide_child_ids.len() + 1);
        assert_eq!(
            inspection_rows[0]["node"]["node_id"],
            fixture.wide_parent_id.to_string()
        );
        assert_eq!(inspection_rows[0]["depth"], 0);
        assert_eq!(inspection_rows[0]["child_count"], 120);
        for (row, expected_id) in inspection_rows[1..].iter().zip(&fixture.wide_child_ids) {
            assert_eq!(row["node"]["node_id"], expected_id.to_string(), "{tool}");
            assert_eq!(row["depth"], 1);
            assert_eq!(row["child_count"], 0);
        }
    }
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_search_exposes_node_type_and_required_tag_filters() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("search-filters.mdtree");
    mdtree_sqlite::import_snapshot_new(&workspace, &mdtree_core::developer_workspace_snapshot())
        .expect("fixture import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("search").with_arguments(
                serde_json::json!({
                    "query":"benchmark",
                    "node_types":["work_item"],
                    "tags":["benchmark"],
                    "statuses":["has_status"],
                    "min_depth":2,
                    "max_depth":2,
                    "created_from":1_750_000_000_000_u64,
                    "updated_to":1_750_000_000_000_u64,
                    "structure":"internal",
                    "limit":1
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("filtered search");
    let result = tool_json(result);
    assert_eq!(result["items"].as_array().expect("items").len(), 1);
    assert_eq!(
        result["items"][0]["title"],
        "T091 — 10,000-Node Benchmark Generator"
    );
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_navigate_selects_every_documented_relation_and_rejects_conflicts() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("navigate-relations.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 8,
            deep_descendants: 3,
            history_revisions: 1,
            relations: 0,
            response_boundary_bytes: 4096,
        },
        917,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");

    for relation in [
        "parent",
        "children",
        "ancestors",
        "descendants",
        "siblings",
        "subtree",
    ] {
        let mut arguments = serde_json::json!({
            "selector":fixture.wide_parent_id,
            "relation":relation
        });
        if !matches!(relation, "parent" | "ancestors") {
            arguments["limit"] = serde_json::json!(2);
        }
        let value = tool_json(
            client
                .peer()
                .call_tool(
                    CallToolRequestParams::new("navigate")
                        .with_arguments(arguments.as_object().expect("arguments").clone()),
                )
                .await
                .unwrap_or_else(|error| panic!("{relation}: {error}")),
        );
        if matches!(relation, "parent" | "ancestors") {
            assert!(value.is_array(), "{relation}");
        } else {
            assert!(value["items"].is_array(), "{relation}");
            assert!(value.get("complete").is_some(), "{relation}");
            assert!(value.get("truncated").is_some(), "{relation}");
        }
    }

    for arguments in [
        serde_json::json!({
            "selector":fixture.wide_parent_id,
            "relation":"children",
            "depth":1
        }),
        serde_json::json!({
            "selector":fixture.wide_parent_id,
            "relation":"parent",
            "limit":2
        }),
    ] {
        client
            .peer()
            .call_tool(
                CallToolRequestParams::new("navigate")
                    .with_arguments(arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect_err("invalid navigate arguments");
    }
    let unknown = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("navigate").with_arguments(
                serde_json::json!({
                    "selector":fixture.wide_parent_id,
                    "relation":"sideways"
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("tool-level validation response");
    assert_eq!(unknown.is_error, Some(true));
    assert!(serde_json::to_string(&unknown)
        .expect("error JSON")
        .contains("unknown variant"));
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_pagination_reports_invalid_limits_and_stale_cursors() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("pagination-errors.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 8,
            deep_descendants: 4,
            history_revisions: 2,
            relations: 4,
            response_boundary_bytes: 4096,
        },
        106,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");

    let invalid = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("children").with_arguments(
                serde_json::json!({"selector":fixture.wide_parent_id,"limit":0})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect_err("invalid limit");
    assert!(invalid.to_string().contains("invalid_limit"));

    let first = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("children").with_arguments(
                serde_json::json!({"selector":fixture.wide_parent_id,"limit":2})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("first page");
    let first_json = serde_json::to_value(first).expect("result JSON");
    let first_value: serde_json::Value = serde_json::from_str(
        first_json["content"][0]["text"]
            .as_str()
            .expect("page JSON text"),
    )
    .expect("page JSON");
    let cursor = first_value["next_cursor"].as_str().expect("cursor");
    let writer = mdtree_sqlite::SqliteStore::open(&workspace).expect("writer");
    writer
        .connection()
        .execute(
            "UPDATE nodes SET updated_at=updated_at+1 WHERE id=?1",
            [fixture.wide_child_ids[0].to_string()],
        )
        .expect("canonical mutation");

    let stale = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("children").with_arguments(
                serde_json::json!({
                    "selector":fixture.wide_parent_id,
                    "limit":2,
                    "cursor":cursor
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect_err("stale cursor");
    assert!(stale.to_string().contains("stale_cursor"));
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_root_node_selectors_and_parent_preserve_single_read_contracts() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("single-node.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 64,
            deep_descendants: 12,
            history_revisions: 10,
            relations: 24,
            response_boundary_bytes: 4096,
        },
        97,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("scale import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");

    let cases = [
        ("root", serde_json::json!({}), fixture.root_id, 1_usize),
        (
            "node",
            serde_json::json!({"selector":fixture.history_node_id}),
            fixture.history_node_id,
            1,
        ),
        (
            "node",
            serde_json::json!({"selector":"history-heavy"}),
            fixture.history_node_id,
            1,
        ),
        (
            "node",
            serde_json::json!({"selector":"/history-heavy"}),
            fixture.history_node_id,
            1,
        ),
        (
            "parent",
            serde_json::json!({"selector":fixture.history_node_id}),
            fixture.root_id,
            1,
        ),
        (
            "parent",
            serde_json::json!({"selector":fixture.root_id}),
            fixture.root_id,
            0,
        ),
    ];
    for (tool, arguments, expected_id, expected_len) in cases {
        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(arguments.as_object().expect("object arguments").clone()),
            )
            .await
            .expect("single-node tool");
        let result_json = serde_json::to_value(result).expect("tool result");
        let value: serde_json::Value = serde_json::from_str(
            result_json["content"][0]["text"]
                .as_str()
                .expect("projection JSON text"),
        )
        .expect("projection JSON");
        let values = value.as_array().expect("projection array");
        assert_eq!(values.len(), expected_len, "{tool} {arguments}");
        if let Some(node) = values.first() {
            assert_eq!(node["id"], serde_json::json!(expected_id));
            assert!(node.get("revision_hash").is_some());
        }
    }
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_prefers_local_workspace_and_uses_configured_fallback_when_missing() {
    let directory = tempdir().expect("tempdir");
    let project = directory.path().join("project");
    std::fs::create_dir(&project).expect("project directory");
    let fallback = directory.path().join("fallback.mdtree");
    mdtree_sqlite::import_snapshot_new(&fallback, &mdtree_core::northstar_platform_snapshot())
        .expect("fallback workspace");

    let fallback_root = root_result(&project, &fallback).await;
    assert!(
        fallback_root.contains("Northstar Platform"),
        "{fallback_root}"
    );

    let local = project.join(".mdtree");
    mdtree_sqlite::import_snapshot_new(&local, &mdtree_core::developer_workspace_snapshot())
        .expect("local workspace");
    let local_root = root_result(&project, &fallback).await;
    assert!(local_root.contains("Developer Workspace"), "{local_root}");
    assert!(!local_root.contains("Northstar Platform"), "{local_root}");
}

#[tokio::test]
async fn real_stdio_switches_between_authorized_workspaces_and_resources_follow() {
    let directory = tempdir().expect("tempdir");
    let first = directory.path().join("first.mdtree");
    let second = directory.path().join("second.mdtree");
    mdtree_sqlite::import_snapshot_new(&first, &mdtree_core::northstar_platform_snapshot())
        .expect("first workspace");
    mdtree_sqlite::import_snapshot_new(&second, &mdtree_core::developer_workspace_snapshot())
        .expect("second workspace");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command
                .arg("--allow-write")
                .arg("--allow-workspace-switch")
                .arg("--workspace-root")
                .arg(directory.path())
                .arg(&first);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let tools = client.peer().list_all_tools().await.expect("tools");
    assert!(tools.iter().any(|tool| tool.name == "switch_workspace"));

    let switched = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("switch_workspace").with_arguments(
                serde_json::json!({"path": second})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("switch workspace");
    assert_ne!(switched.is_error, Some(true));
    let root = client
        .peer()
        .call_tool(CallToolRequestParams::new("root"))
        .await
        .expect("root after switch");
    assert!(serde_json::to_string(&root)
        .expect("root JSON")
        .contains("Developer Workspace"));
    let resource = client
        .peer()
        .read_resource(ReadResourceRequestParams::new("mdtree://workspace"))
        .await
        .expect("workspace resource");
    assert!(serde_json::to_string(&resource)
        .expect("resource JSON")
        .contains(
            second
                .canonicalize()
                .expect("canonical second")
                .to_str()
                .expect("UTF-8 path")
        ));

    let missing = directory.path().join("new.mdtree");
    let selected_missing = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("switch_workspace").with_arguments(
                serde_json::json!({"path": missing})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("select missing workspace");
    assert_ne!(selected_missing.is_error, Some(true));
    let initialized = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("initialize_workspace").with_arguments(
                serde_json::json!({"name":"Switched Workspace"})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("initialize selected workspace");
    assert_ne!(initialized.is_error, Some(true));
    assert!(missing.is_file());
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_write_mode_can_initialize_a_missing_workspace() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("new.mdtree");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg("--allow-write").arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");

    let before = client
        .peer()
        .call_tool(CallToolRequestParams::new("root"))
        .await
        .expect_err("root must fail before initialization");
    assert!(before.to_string().contains("not initialized"));
    let initialized = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("initialize_workspace").with_arguments(
                serde_json::json!({"name":"Agent Knowledge"})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("initialize workspace");
    assert_ne!(initialized.is_error, Some(true));
    assert!(workspace.is_file());

    let root = client
        .peer()
        .call_tool(CallToolRequestParams::new("root"))
        .await
        .expect("root after initialization");
    assert_ne!(root.is_error, Some(true));
    assert!(serde_json::to_string(&root)
        .expect("root JSON")
        .contains("Agent Knowledge"));
    let repeated = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("initialize_workspace").with_arguments(
                serde_json::json!({"name":"Replacement"})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect_err("repeated initialization must fail");
    assert!(repeated.to_string().contains("already initialized"));

    client.cancel().await.expect("shutdown");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn real_stdio_binary_initializes_and_serves_northstar_platform() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("northstar.mdtree");
    mdtree_sqlite::import_snapshot_new(&workspace, &mdtree_core::northstar_platform_snapshot())
        .expect("Northstar Platform");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let tools = client.peer().list_all_tools().await.expect("tools");
    let root_id = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";
    let cases = vec![
        ("root", serde_json::json!({})),
        ("workspace_status", serde_json::json!({})),
        ("validate", serde_json::json!({})),
        ("node", serde_json::json!({"selector":root_id})),
        ("batch_nodes", serde_json::json!({"selectors":[root_id]})),
        (
            "batch_children",
            serde_json::json!({"requests":[{"parent":root_id,"limit":1}]}),
        ),
        ("statistics", serde_json::json!({"selector":root_id})),
        (
            "contains",
            serde_json::json!({"ancestor":root_id,"descendant":root_id}),
        ),
        (
            "lowest_common_ancestor",
            serde_json::json!({"left":root_id,"right":root_id}),
        ),
        (
            "path_between",
            serde_json::json!({"left":root_id,"right":root_id}),
        ),
        (
            "distance",
            serde_json::json!({"left":root_id,"right":root_id}),
        ),
        ("children", serde_json::json!({"selector":root_id})),
        ("parent", serde_json::json!({"selector":root_id})),
        ("ancestors", serde_json::json!({"selector":root_id})),
        ("descendants", serde_json::json!({"selector":root_id})),
        ("siblings", serde_json::json!({"selector":root_id})),
        ("subtree", serde_json::json!({"selector":root_id})),
        (
            "filter_nodes",
            serde_json::json!({"selector":root_id,"predicate":"leaf"}),
        ),
        ("child_at", serde_json::json!({"parent":root_id,"index":0})),
        ("adjacent_siblings", serde_json::json!({"selector":root_id})),
        ("history", serde_json::json!({"selector":root_id})),
        (
            "revision",
            serde_json::json!({"selector":root_id,"version":1}),
        ),
        (
            "version_diff",
            serde_json::json!({"selector":root_id,"from":1,"to":1}),
        ),
        (
            "subtree_diff",
            serde_json::json!({
                "from_selector":root_id,
                "to_selector":"architecture",
                "limit":20
            }),
        ),
        (
            "navigate",
            serde_json::json!({"selector":root_id,"depth":2,"limit":20}),
        ),
        ("path", serde_json::json!({"selector":root_id})),
        (
            "search",
            serde_json::json!({"query":"domain events kafka","limit":10}),
        ),
        (
            "locate",
            serde_json::json!({"query":"Add architecture decision for API retries","node_type":"architecture_decision"}),
        ),
        (
            "inspect",
            serde_json::json!({"selector":root_id,"depth":2,"limit":20}),
        ),
        (
            "examples",
            serde_json::json!({"selector":"payments-service","limit":3}),
        ),
        (
            "read_context",
            serde_json::json!({"selector":root_id,"byte_limit":8192}),
        ),
        (
            "write_context",
            serde_json::json!({"selector":root_id,"byte_limit":8192}),
        ),
        (
            "references",
            serde_json::json!({"selector":"payments-service"}),
        ),
        (
            "backlinks",
            serde_json::json!({"selector":"domain-events-via-kafka"}),
        ),
        (
            "resolve_reference",
            serde_json::json!({"target":"ADR-002 — Domain Events via Kafka"}),
        ),
    ];
    assert_eq!(tools.len(), cases.len());
    for (name, arguments) in cases {
        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new(name)
                    .with_arguments(arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect("tool");
        assert_ne!(result.is_error, Some(true), "{name} failed");
    }
    for uri in [
        "mdtree://workspace",
        "mdtree://tree",
        "mdtree://references",
        "mdtree://node/01JZ8Q5CWPN8T7KPN5A1V9B6XM",
        "mdtree://node/01JZ8Q5CWPN8T7KPN5A1V9B6XM/section/northstar-platform",
    ] {
        let resource = client
            .peer()
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .expect("resource");
        assert_eq!(resource.contents.len(), 1);
    }
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn real_stdio_write_mode_requires_explicit_flag() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("northstar.mdtree");
    mdtree_sqlite::import_snapshot_new(&workspace, &mdtree_core::northstar_platform_snapshot())
        .expect("Northstar Platform");
    let rollback_id = "01JZ8Q5CWPN8T7KPN5A1V9B6XS";
    let rollback_store = mdtree_sqlite::SqliteStore::open(&workspace).expect("rollback fixture");
    rollback_store
        .connection()
        .execute_batch(&format!(
            "CREATE TRIGGER mcp_rollback_injection BEFORE UPDATE ON nodes
             WHEN NEW.id='{rollback_id}' BEGIN SELECT RAISE(ABORT,'injected rollback'); END;"
        ))
        .expect("rollback trigger");
    drop(rollback_store);
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg("--allow-write").arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let tools = client.peer().list_all_tools().await.expect("tools");
    assert!(tools
        .iter()
        .any(|tool| tool.name == "mutation_capabilities"));
    let capability = client
        .peer()
        .call_tool(CallToolRequestParams::new("mutation_capabilities"))
        .await
        .expect("capability tool");
    assert_ne!(capability.is_error, Some(true));

    let clone_arguments = serde_json::json!({
        "operation_id":"clone-architecture-decisions",
        "source":"architecture-decisions",
        "destination_parent":"01JZ8Q5CWPN8T7KPN5A1V9B6XM",
        "precondition":{"expected_version":1},
        "options":{"author":"mcp-test","change_summary":"Clone decisions"}
    });
    let cloned = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("clone_subtree")
                .with_arguments(clone_arguments.as_object().expect("arguments").clone()),
        )
        .await
        .expect("clone subtree");
    let cloned = tool_json(cloned);
    assert_eq!(cloned["status"], "applied");
    assert_eq!(cloned["node_count"], 4);
    let clone_id = cloned["cloned_root_id"].clone();
    let replayed = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("clone_subtree")
                .with_arguments(clone_arguments.as_object().expect("arguments").clone()),
        )
        .await
        .expect("clone replay");
    assert_eq!(tool_json(replayed)["cloned_root_id"], clone_id);

    let export_directory = directory.path().join("mcp-export");
    std::fs::create_dir(&export_directory).expect("MCP export directory");
    let exported = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("export_node").with_arguments(
                serde_json::json!({
                    "selector": "architecture",
                    "destination": export_directory.clone(),
                    "subtree": true,
                    "depth": 1
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("export node");
    assert_ne!(exported.is_error, Some(true));
    assert!(export_directory.join("architecture.md").is_file());
    assert!(export_directory
        .join("architecture-decisions/architecture-decisions.md")
        .is_file());
    assert!(!export_directory
        .join("architecture-decisions/postgresql-as-system-of-record")
        .exists());
    let export_file = directory.path().join("mcp-single.md");
    let exported_file = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("export_node").with_arguments(
                serde_json::json!({
                    "selector": "services",
                    "destination": export_file.clone()
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("export node file");
    assert_ne!(exported_file.is_error, Some(true));
    assert!(export_file.is_file());
    let invalid_export = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("export_node").with_arguments(
                serde_json::json!({
                    "selector": "architecture",
                    "destination": directory.path().join("invalid-export"),
                    "depth": 1
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await;
    assert!(
        invalid_export.is_err(),
        "depth without subtree unexpectedly succeeded"
    );

    let root_id = "01JZ8Q5CWPN8T7KPN5A1V9B6XM";
    let first_id = "01JZ8Q5CWPN8T7KPN5A1V9B700";
    let batch_parent_id = "01JZ8Q5CWPN8T7KPN5A1V9B710";
    let batch_child_id = "01JZ8Q5CWPN8T7KPN5A1V9B711";
    let batch_removed_id = "01JZ8Q5CWPN8T7KPN5A1V9B712";
    let generic_batch = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("mutation_batch").with_arguments(
                serde_json::json!({
                    "operation_id":"stdio-generic-batch",
                    "operations":[
                        {"kind":"create","label":"batch-parent","parent":root_id,"title":"Batch Parent","requested_id":batch_parent_id},
                        {"kind":"create","label":"batch-child","parent":"batch-parent","title":"Batch Child","requested_id":batch_child_id},
                        {"kind":"create","label":"batch-removed","parent":root_id,"title":"Batch Removed","requested_id":batch_removed_id}
                    ],
                    "options":{"author":"stdio-test"}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("heterogeneous mutation batch");
    assert_eq!(tool_json(generic_batch)["operation_count"], 3);
    let focused_batch = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("atomic_tree_batch").with_arguments(
                serde_json::json!({
                    "operation_id":"stdio-focused-tree-batch",
                    "moves":[{"selector":batch_child_id,"destination_parent":root_id,"precondition":{"expected_version":1}}],
                    "removals":[{"selector":batch_removed_id,"expected_version":1,"confirm":true}],
                    "options":{"author":"stdio-test"}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("focused tree batch");
    let focused_batch = tool_json(focused_batch);
    assert_eq!(focused_batch["status"], "applied");
    assert_eq!(focused_batch["removed_node_count"], 1);
    let dry_run = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("create_node").with_arguments(
                serde_json::json!({
                    "parent": root_id,
                    "title": "Mutable MCP",
                    "content": "# Mutable MCP\n",
                    "requested_id": first_id,
                    "options": {"dry_run": true, "author": "stdio-test"}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("dry-run create");
    assert_ne!(dry_run.is_error, Some(true));

    let create_arguments = serde_json::json!({
        "operation_id": "create-mutable-mcp",
        "parent": root_id,
        "title": "Mutable MCP",
        "content": "# Mutable MCP\n",
        "requested_id": first_id,
        "options": {"author": "stdio-test", "change_summary": "stdio create"}
    });
    let created = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("create_node")
                .with_arguments(create_arguments.as_object().expect("arguments").clone()),
        )
        .await
        .expect("create");
    assert_ne!(created.is_error, Some(true));
    let replayed_create = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("create_node")
                .with_arguments(create_arguments.as_object().expect("arguments").clone()),
        )
        .await
        .expect("replayed create");
    assert_ne!(replayed_create.is_error, Some(true));
    let mismatched_create = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("create_node").with_arguments(
                serde_json::json!({
                    "operation_id": "create-mutable-mcp",
                    "parent": root_id,
                    "title": "Different payload",
                    "requested_id": first_id,
                    "options": {}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await;
    assert!(mismatched_create.is_err());
    let read_created = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("node").with_arguments(
                serde_json::json!({"selector": first_id})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("read created node");
    assert_ne!(read_created.is_error, Some(true));

    let collision = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("create_node").with_arguments(
                serde_json::json!({
                    "parent": root_id,
                    "title": "Mutable MCP",
                    "requested_id": "01JZ8Q5CWPN8T7KPN5A1V9B701",
                    "options": {}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("slug collision create");
    assert_ne!(collision.is_error, Some(true));

    let subtree_arguments = serde_json::json!({
        "operation_id": "create-mcp-subtree",
        "parent": root_id,
        "title": "Subtree Root",
        "requested_id": "01JZ8Q5CWPN8T7KPN5A1V9B702",
        "children": [{
            "title": "Subtree Branch",
            "requested_id": "01JZ8Q5CWPN8T7KPN5A1V9B703",
            "children": [{
                "title": "Subtree Leaf",
                "content": "# Subtree Leaf\n\nCreated atomically.\n",
                "requested_id": "01JZ8Q5CWPN8T7KPN5A1V9B704"
            }]
        }],
        "options": {"author": "stdio-test", "change_summary": "stdio subtree"}
    });
    let subtree = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("create_node")
                .with_arguments(subtree_arguments.as_object().expect("arguments").clone()),
        )
        .await
        .expect("create subtree");
    assert_ne!(subtree.is_error, Some(true));
    let subtree_json = serde_json::to_string(&subtree).expect("subtree JSON");
    assert!(subtree_json.contains("northstar-platform/subtree-root"));
    assert!(subtree_json.contains("northstar-platform/subtree-root/subtree-branch"));
    assert!(subtree_json.contains("northstar-platform/subtree-root/subtree-branch/subtree-leaf"));
    let replayed_subtree = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("create_node")
                .with_arguments(subtree_arguments.as_object().expect("arguments").clone()),
        )
        .await
        .expect("replayed subtree");
    assert_eq!(
        serde_json::to_string(&replayed_subtree).expect("replayed subtree JSON"),
        subtree_json
    );
    let leaf = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("node").with_arguments(
                serde_json::json!({"selector":"01JZ8Q5CWPN8T7KPN5A1V9B704"})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("read subtree leaf");
    assert!(serde_json::to_string(&leaf)
        .expect("leaf JSON")
        .contains("Created atomically"));

    let update_plan = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("update_node").with_arguments(
                serde_json::json!({
                    "selector": first_id,
                    "content": "# Mutable MCP\n\nAtomic update marker.\n",
                    "precondition": {"expected_version": 1},
                    "options": {"dry_run": true, "author": "stdio-test"}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("update dry run");
    assert_ne!(update_plan.is_error, Some(true));

    let update_arguments = serde_json::json!({
        "operation_id": "update-mutable-mcp",
        "selector": first_id,
        "content": "# Mutable MCP\n\nAtomic update marker.\n",
        "metadata": {"title": "Ignored rename", "summary": "Updated over MCP"},
        "precondition": {"expected_version": 1},
        "options": {"author": "stdio-test", "change_summary": "stdio update"}
    });
    let updated = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("update_node")
                .with_arguments(update_arguments.as_object().expect("arguments").clone()),
        )
        .await
        .expect("update");
    assert_ne!(updated.is_error, Some(true));
    let replayed_update = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("update_node")
                .with_arguments(update_arguments.as_object().expect("arguments").clone()),
        )
        .await
        .expect("replayed update");
    assert_ne!(replayed_update.is_error, Some(true));

    let no_op = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("update_node").with_arguments(
                serde_json::json!({
                    "selector": first_id,
                    "content": "# Mutable MCP\n\nAtomic update marker.\n",
                    "metadata": {"title": "Mutable MCP", "summary": "Updated over MCP"},
                    "precondition": {"expected_version": 2},
                    "options": {}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("no-op update");
    assert_ne!(no_op.is_error, Some(true));

    let content_hash = hex_hash(mdtree_core::hash_content(
        "# Mutable MCP\n\nAtomic update marker.\n",
    ));
    let hash_guarded = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("update_node").with_arguments(
                serde_json::json!({
                    "selector": first_id,
                    "precondition": {"expected_content_hash": content_hash},
                    "options": {"dry_run": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("hash-guarded update");
    assert_ne!(hash_guarded.is_error, Some(true));

    let rename_plan = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("rename_node").with_arguments(
                serde_json::json!({
                    "selector": first_id,
                    "title": "Architecture",
                    "slug_policy": "regenerate",
                    "precondition": {"expected_version": 2},
                    "options": {"dry_run": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("rename collision plan");
    assert_ne!(rename_plan.is_error, Some(true));

    let rename_arguments = serde_json::json!({
        "operation_id": "rename-mutable-mcp",
        "selector": first_id,
        "title": "Renamed MCP",
        "slug_policy": "preserve",
        "precondition": {"expected_version": 2},
        "options": {"author": "stdio-test", "change_summary": "stdio rename"}
    });
    for attempt in 0..2 {
        let renamed = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("rename_node")
                    .with_arguments(rename_arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect("rename/replay");
        assert_ne!(renamed.is_error, Some(true), "rename attempt {attempt}");
    }
    let renamed_path = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("path").with_arguments(
                serde_json::json!({"selector": first_id})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("renamed path");
    assert_ne!(renamed_path.is_error, Some(true));

    let move_plan = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("move_node").with_arguments(
                serde_json::json!({
                    "selector": first_id,
                    "destination_parent": "services",
                    "precondition": {"expected_version": 3},
                    "options": {"dry_run": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("move plan");
    assert_ne!(move_plan.is_error, Some(true));

    let move_arguments = serde_json::json!({
        "operation_id": "move-mutable-mcp",
        "selector": first_id,
        "destination_parent": "services",
        "precondition": {"expected_version": 3},
        "options": {"author": "stdio-test", "change_summary": "stdio move"}
    });
    for attempt in 0..2 {
        let moved = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("move_node")
                    .with_arguments(move_arguments.as_object().expect("arguments").clone()),
            )
            .await
            .expect("move/replay");
        assert_ne!(moved.is_error, Some(true), "move attempt {attempt}");
    }

    let reordered = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("reorder_node").with_arguments(
                serde_json::json!({
                    "operation_id": "reorder-mutable-mcp",
                    "selector": first_id,
                    "sibling_order": 0,
                    "precondition": {"expected_version": 4},
                    "options": {"author": "stdio-test", "validate_after_write": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("reorder");
    assert_ne!(reordered.is_error, Some(true));
    let reordered = tool_json(reordered);
    assert_eq!(reordered["node"]["sibling_order"], 0);
    let children = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("children").with_arguments(
                serde_json::json!({"selector":"services","limit":100})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("children after reorder");
    let children = tool_json(children);
    assert_eq!(children["items"][0]["id"], first_id);
    assert_eq!(children["items"][0]["sibling_order"], 0);

    let root_move = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("move_node").with_arguments(
                serde_json::json!({
                    "selector": root_id,
                    "destination_parent": first_id,
                    "precondition": {"expected_version": 1},
                    "options": {"dry_run": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await;
    assert!(root_move.is_err());
    let cyclic_move = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("move_node").with_arguments(
                serde_json::json!({
                    "selector": first_id,
                    "destination_parent": first_id,
                    "precondition": {"expected_version": 5},
                    "options": {"dry_run": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await;
    assert!(cyclic_move.is_err());

    for (mode, expected_version, references) in [
        (
            "add",
            5,
            serde_json::json!([{"reference_type":"depends_on","target":"domain-events-via-kafka"}]),
        ),
        (
            "remove",
            6,
            serde_json::json!([{"reference_type":"depends_on","target":"domain-events-via-kafka"}]),
        ),
        (
            "set",
            7,
            serde_json::json!([{"reference_type":"related_to","target":"Legacy system"}]),
        ),
    ] {
        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("set_references").with_arguments(
                    serde_json::json!({
                        "selector": first_id,
                        "mode": mode,
                        "references": references,
                        "precondition": {"expected_version": expected_version},
                        "options": {"author": "stdio-test"}
                    })
                    .as_object()
                    .expect("arguments")
                    .clone(),
                ),
            )
            .await
            .expect("reference mutation");
        assert_ne!(result.is_error, Some(true), "reference mode {mode}");
    }

    let last_service_index = children["items"]
        .as_array()
        .expect("service children")
        .len()
        .saturating_sub(1);
    assert!(last_service_index > 0);
    let same_parent_move = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("move_node").with_arguments(
                serde_json::json!({
                    "selector":first_id,
                    "destination_parent":"services",
                    "sibling_order":last_service_index,
                    "precondition":{"expected_version":8},
                    "options":{"author":"stdio-test"}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("same-parent move");
    let same_parent_move = tool_json(same_parent_move);
    assert_eq!(
        same_parent_move["node"]["sibling_order"],
        last_service_index
    );
    let references = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("references").with_arguments(
                serde_json::json!({"selector":first_id,"limit":100})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            ),
        )
        .await
        .expect("references after same-parent move");
    assert_eq!(
        tool_json(references)["items"]
            .as_array()
            .expect("references")
            .len(),
        1
    );

    let restore_plan = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("restore_version").with_arguments(
                serde_json::json!({
                    "selector": first_id,
                    "target_version": 1,
                    "precondition": {"expected_version": 9},
                    "options": {"dry_run": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("restore plan");
    assert_ne!(restore_plan.is_error, Some(true));
    let restored = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("restore_version").with_arguments(
                serde_json::json!({
                    "operation_id": "restore-mutable-mcp",
                    "selector": first_id,
                    "target_version": 1,
                    "precondition": {"expected_version": 9},
                    "options": {"author": "stdio-test", "validate_after_write": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("restore");
    assert_ne!(restored.is_error, Some(true));

    let removal_plan = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("remove_node").with_arguments(
                serde_json::json!({
                    "selector": "01JZ8Q5CWPN8T7KPN5A1V9B701",
                    "expected_version": 1,
                    "options": {"dry_run": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("removal plan");
    assert_ne!(removal_plan.is_error, Some(true));
    let removed = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("remove_node").with_arguments(
                serde_json::json!({
                    "operation_id": "remove-collision-node",
                    "selector": "01JZ8Q5CWPN8T7KPN5A1V9B701",
                    "expected_version": 1,
                    "confirm": true,
                    "options": {"validate_after_write": true}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect("remove");
    assert_ne!(removed.is_error, Some(true));
    let root_remove = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("remove_node").with_arguments(
                serde_json::json!({
                    "selector": root_id,
                    "expected_version": 1,
                    "confirm": true,
                    "options": {}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await;
    assert!(root_remove.is_err());

    let stale = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("update_node").with_arguments(
                serde_json::json!({
                    "selector": first_id,
                    "content": "stale",
                    "precondition": {"expected_version": 1},
                    "options": {}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await;
    assert!(stale.is_err(), "stale update unexpectedly succeeded");
    let injected = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("update_node").with_arguments(
                serde_json::json!({
                    "selector": rollback_id,
                    "content": "# Must roll back\n",
                    "precondition": {"expected_version": 1},
                    "options": {}
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await;
    assert!(
        injected.is_err(),
        "injected transaction unexpectedly committed"
    );
    client.cancel().await.expect("shutdown");
    let rollback_store =
        mdtree_sqlite::SqliteStore::open(&workspace).expect("reopen rollback fixture");
    let rollback_node = rollback_store
        .get(rollback_id.parse().expect("rollback ID"))
        .expect("rollback read")
        .expect("rollback node");
    assert_eq!(rollback_node.fields().version, 1);
    assert_eq!(
        rollback_store
            .revisions(rollback_node.id())
            .expect("rollback history")
            .len(),
        1
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn real_stdio_search_continuation_enumerates_stable_ranking_ties() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("search-pages.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 120,
            deep_descendants: 0,
            history_revisions: 1,
            relations: 0,
            response_boundary_bytes: 256,
        },
        912,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let store = mdtree_sqlite::SqliteStore::open(&workspace).expect("store");
    for child in &fixture.wide_child_ids {
        store
            .connection()
            .execute(
                "INSERT INTO \"references\" (source_node_id,target_node_id,target_ref,reference_type,origin) VALUES (?1,?2,?2,'mcp_search_scope','explicit')",
                [fixture.wide_parent_id.to_string(), child.to_string()],
            )
            .expect("reference");
    }
    drop(store);

    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
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
        let mut cursor: Option<String> = None;
        let mut ids = Vec::new();
        loop {
            let page = tool_json(
                client
                    .peer()
                    .call_tool(
                        CallToolRequestParams::new("search").with_arguments(
                            serde_json::json!({
                                "query":"deterministic wide fixture",
                                "scope":scope,
                                "scope_node":scope_node,
                                "limit":37,
                                "cursor":cursor
                            })
                            .as_object()
                            .expect("arguments")
                            .clone(),
                        ),
                    )
                    .await
                    .expect("search page"),
            );
            ids.extend(
                page["items"]
                    .as_array()
                    .expect("items")
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

    let first = tool_json(
        client
            .peer()
            .call_tool(
                CallToolRequestParams::new("search").with_arguments(
                    serde_json::json!({"query":"deterministic wide fixture","limit":2})
                        .as_object()
                        .expect("arguments")
                        .clone(),
                ),
            )
            .await
            .expect("first page"),
    );
    let changed = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("search").with_arguments(
                serde_json::json!({
                    "query":"different query",
                    "limit":2,
                    "cursor":first["next_cursor"].as_str().expect("cursor")
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect_err("changed query continuation");
    assert!(changed.to_string().contains("invalid_cursor"));
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn real_stdio_reference_continuation_preserves_complete_typed_sets() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("reference-pages.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 175,
            deep_descendants: 0,
            history_revisions: 1,
            relations: 0,
            response_boundary_bytes: 256,
        },
        914,
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

    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    for (tool, relation) in [
        ("references", "outgoing_page"),
        ("backlinks", "backlink_page"),
    ] {
        let mut cursor: Option<String> = None;
        let mut items = Vec::new();
        loop {
            let page = tool_json(
                client
                    .peer()
                    .call_tool(
                        CallToolRequestParams::new(tool).with_arguments(
                            serde_json::json!({
                                "selector":fixture.root_id,
                                "limit":37,
                                "cursor":cursor
                            })
                            .as_object()
                            .expect("arguments")
                            .clone(),
                        ),
                    )
                    .await
                    .expect("reference page"),
            );
            items.extend(page["items"].as_array().expect("items").iter().cloned());
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(items.len(), 175, "{tool}");
        assert!(items.iter().all(|item| item["reference_type"] == relation));
        assert_eq!(
            items
                .iter()
                .map(|item| item["metadata"]["fixture_index"].as_u64().expect("index"))
                .collect::<Vec<_>>(),
            (0..175).collect::<Vec<_>>()
        );
        if tool == "references" {
            assert_eq!(items[174]["target"]["status"], "unresolved");
        }
    }
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_byte_limit_returns_resumable_largest_prefix() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("byte-bounded-pages.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 0,
            deep_descendants: 0,
            history_revisions: 1,
            relations: 0,
            response_boundary_bytes: 600_000,
        },
        915,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let mut cursor: Option<String> = None;
    let mut ids = Vec::new();
    let mut page_lengths = Vec::new();
    loop {
        let result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("children").with_arguments(
                    serde_json::json!({
                        "selector":fixture.root_id,
                        "limit":5,
                        "cursor":cursor
                    })
                    .as_object()
                    .expect("arguments")
                    .clone(),
                ),
            )
            .await
            .expect("children page");
        let encoded = serde_json::to_value(&result).expect("tool result JSON");
        let text = encoded["content"][0]["text"]
            .as_str()
            .expect("tool JSON text");
        page_lengths.push(text.len());
        let page: serde_json::Value = serde_json::from_str(text).expect("page JSON");
        ids.extend(
            page["items"]
                .as_array()
                .expect("items")
                .iter()
                .map(|item| item["id"].as_str().expect("node ID").to_owned()),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    let expected = fixture
        .snapshot
        .nodes
        .iter()
        .filter(|node| node.parent_id == Some(fixture.root_id))
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids, expected);
    assert_eq!(page_lengths.len(), 2);
    assert!(page_lengths.iter().all(|length| *length <= 1_048_576));
    client.cancel().await.expect("shutdown");
}

#[tokio::test]
async fn real_stdio_byte_limit_reports_single_item_oversize_explicitly() {
    let directory = tempdir().expect("tempdir");
    let workspace = directory.path().join("single-item-oversize.mdtree");
    let fixture = mdtree_core::generate_large_tree_fixture(
        mdtree_core::LargeTreeFixtureSpec {
            wide_children: 0,
            deep_descendants: 0,
            history_revisions: 1,
            relations: 0,
            response_boundary_bytes: 1_048_576,
        },
        916,
    );
    mdtree_sqlite::import_snapshot_new(&workspace, &fixture.snapshot).expect("fixture import");
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_mdtree-mcp")).configure(|command| {
            command.arg(&workspace);
        }),
    )
    .expect("child transport");
    let client = ().serve(transport).await.expect("protocol initialize");
    let first = tool_json(
        client
            .peer()
            .call_tool(
                CallToolRequestParams::new("children").with_arguments(
                    serde_json::json!({"selector":fixture.root_id,"limit":5})
                        .as_object()
                        .expect("arguments")
                        .clone(),
                ),
            )
            .await
            .expect("bounded prefix"),
    );
    let error = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("children").with_arguments(
                serde_json::json!({
                    "selector":fixture.root_id,
                    "limit":5,
                    "cursor":first["next_cursor"].as_str().expect("cursor")
                })
                .as_object()
                .expect("arguments")
                .clone(),
            ),
        )
        .await
        .expect_err("single oversized item");
    assert!(error.to_string().contains("item_too_large"));
    client.cancel().await.expect("shutdown");
}

fn hex_hash(hash: mdtree_core::NodeHash) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in hash.as_bytes() {
        write!(&mut encoded, "{byte:02x}").expect("string write");
    }
    encoded
}
