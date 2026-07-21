//! Canonical Northstar Platform interoperability and service integration coverage.

use std::path::PathBuf;
use std::str::FromStr;

use mdtree_core::{
    LocateAction, LocateStatus, NodeId, NodeType, SearchFilters, SearchRequest, SearchScope,
    SystemUlidGenerator,
};
use mdtree_sqlite::{
    backup_workspace, check_workspace, export_snapshot, import_markdown_snapshot_new,
    import_snapshot_new, plan_json_import, plan_markdown_import, restore_workspace, CheckStatus,
    SqliteStore,
};
use tempfile::tempdir;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn corruption_rebuild_backup_restore_and_invalid_restore_are_atomic() {
    let directory = tempdir().expect("tempdir");
    let source = directory.path().join("source.mdtree");
    import_snapshot_new(&source, &mdtree_core::northstar_platform_snapshot()).expect("import");
    let mut store = SqliteStore::open(&source).expect("workspace");
    let backup = directory.path().join("backup.mdtree");
    backup_workspace(&store, &backup).expect("backup");
    store
        .connection()
        .execute("DELETE FROM sections", [])
        .expect("corrupt derived rows");
    assert_eq!(
        check_workspace(&store).expect("check").status,
        CheckStatus::Invalid
    );
    store
        .rebuild_derived(&SystemUlidGenerator)
        .expect("rebuild");
    assert_eq!(
        check_workspace(&store).expect("check").status,
        CheckStatus::Healthy
    );
    let restored = directory.path().join("restored.mdtree");
    restore_workspace(&backup, &restored, false).expect("restore");
    assert_eq!(
        check_workspace(&SqliteStore::open(&restored).expect("restored"))
            .expect("check")
            .status,
        CheckStatus::Healthy
    );
    let invalid = directory.path().join("invalid.mdtree");
    std::fs::write(&invalid, b"invalid database").expect("invalid fixture");
    let before = std::fs::read(&restored).expect("before");
    assert!(restore_workspace(&invalid, &restored, true).is_err());
    assert_eq!(std::fs::read(&restored).expect("after"), before);
}

#[test]
fn checked_in_json_and_markdown_are_equivalent_and_round_trip() {
    let json = std::fs::read(root().join("examples/northstar-platform.snapshot.json"))
        .expect("JSON fixture");
    let json_plan = plan_json_import(&json).expect("JSON plan");
    let markdown_plan = plan_markdown_import(&root().join("examples/northstar-platform-markdown"))
        .expect("Markdown plan");
    assert!(json_plan.validation.is_valid());
    assert!(markdown_plan.validation.is_valid());
    assert_eq!(json_plan.snapshot, markdown_plan.snapshot);
    assert_eq!(json_plan.snapshot.nodes.len(), 11);
    assert_eq!(json_plan.snapshot.references.len(), 5);

    let directory = tempdir().expect("tempdir");
    let json_workspace = directory.path().join("json.mdtree");
    let markdown_workspace = directory.path().join("markdown.mdtree");
    import_snapshot_new(&json_workspace, &json_plan.snapshot).expect("JSON import");
    import_markdown_snapshot_new(
        &root().join("examples/northstar-platform-markdown"),
        &markdown_workspace,
    )
    .expect("Markdown import");
    let json_export = export_snapshot(&SqliteStore::open(&json_workspace).expect("JSON workspace"))
        .expect("export");
    let markdown_export =
        export_snapshot(&SqliteStore::open(&markdown_workspace).expect("Markdown workspace"))
            .expect("export");
    assert_eq!(json_export.nodes, markdown_export.nodes);
    assert_eq!(json_export.references, markdown_export.references);
}

#[test]
fn navigation_references_search_location_and_context_match_specification() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("northstar.mdtree");
    import_snapshot_new(&path, &mdtree_core::northstar_platform_snapshot()).expect("import");
    let store = SqliteStore::open(&path).expect("workspace");
    let decisions = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XP").expect("decisions");
    let kafka = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XR").expect("Kafka ADR");
    let payments = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XX").expect("payments");
    assert_eq!(store.children(decisions).expect("ADR children").len(), 3);
    assert_eq!(
        store.breadcrumb(kafka).expect("breadcrumb").to_string(),
        "Northstar Platform > Architecture > Architecture Decisions > ADR-002 — Domain Events via Kafka"
    );
    assert_eq!(
        store
            .outgoing_references(payments)
            .expect("references")
            .len(),
        2
    );
    assert_eq!(store.backlinks(kafka).expect("backlinks").len(), 3);
    assert!(store
        .unresolved_references()
        .expect("unresolved")
        .is_empty());

    let results = store
        .search_content(&SearchRequest {
            query: "domain events kafka".into(),
            scope: SearchScope::Workspace,
            scope_node: None,
            filters: SearchFilters::default(),
            limit: 10,
            offset: 0,
            prefix_last_token: true,
        })
        .expect("search");
    assert_eq!(results[0].node_id, kafka);
    let located = store
        .locate_target(
            "Add architecture decision for API retries",
            Some(&NodeType::from_str("architecture_decision").expect("node type")),
        )
        .expect("locate");
    assert_eq!(located.status, LocateStatus::Recommended);
    assert_eq!(located.action, Some(LocateAction::CreateChild));
    assert_eq!(located.candidates[0].result.node_id, decisions);
    assert_eq!(
        located.suggested_title.as_deref(),
        Some("Architecture Decision For API Retries")
    );
    assert_eq!(
        store
            .read_context(payments, 8_192)
            .expect("read context")
            .references
            .len(),
        2
    );
    assert!(
        store
            .write_context(decisions, 8_192)
            .expect("write context")
            .sibling_examples
            .len()
            <= 3
    );
}
