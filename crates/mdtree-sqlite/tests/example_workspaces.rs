//! Contract coverage for the generated developer workspace example.

use std::path::PathBuf;

use mdtree_core::{developer_workspace_snapshot, NodeSelector, ReferenceTarget};
use mdtree_sqlite::{check_workspace, export_snapshot, workspace_status, CheckStatus, SqliteStore};

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

#[test]
fn checked_in_developer_workspace_is_healthy_and_matches_the_approved_shape() {
    let developer_path = example("developer-workspace.mdtree");
    let developer = SqliteStore::open(&developer_path).expect("developer example");
    assert_eq!(
        check_workspace(&developer).expect("developer check").status,
        CheckStatus::Healthy
    );
    let status =
        workspace_status(developer.connection(), &developer_path).expect("developer status");
    assert_eq!(status.node_count, 52);
    assert_eq!(status.reference_count, 8);
    let exported = export_snapshot(&developer).expect("developer export");
    let expected = developer_workspace_snapshot();
    assert_eq!(exported.nodes, expected.nodes);
    assert!(expected
        .references
        .iter()
        .all(|reference| exported.references.contains(reference)));
}

#[test]
fn developer_workspace_preserves_documented_structure_and_relations() {
    let developer =
        SqliteStore::open(&example("developer-workspace.mdtree")).expect("developer example");
    let t091 = developer
        .resolve(&NodeSelector::Slug(
            "t091-10-000-node-benchmark-generator"
                .parse()
                .expect("slug"),
        ))
        .expect("resolve T091")
        .expect("T091 node");
    assert!(developer
        .canonical_path(t091.id())
        .expect("T091 path")
        .iter()
        .any(|slug| slug.as_str() == "features"));
    let references = developer
        .outgoing_references(t091.id())
        .expect("T091 references");
    assert_eq!(references.len(), 2);
    assert!(references
        .iter()
        .all(|reference| matches!(reference.target, ReferenceTarget::Resolved { .. })));

    let archived = developer
        .resolve(&NodeSelector::Slug(
            "arch-001-initial-architecture-planning"
                .parse()
                .expect("slug"),
        ))
        .expect("resolve archived feature")
        .expect("archived feature");
    assert!(developer
        .canonical_path(archived.id())
        .expect("archived path")
        .iter()
        .any(|slug| slug.as_str() == "archived-features"));
}
