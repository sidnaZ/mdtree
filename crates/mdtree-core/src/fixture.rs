//! Canonical reference fixtures shared by adapters and integration tests.

use std::collections::BTreeMap;
use std::str::FromStr;

use serde_json::Value;

use crate::{
    hash_content, hash_revision, NodeId, NodeMetadata, NodeType, Reference, ReferenceOrigin,
    ReferenceTarget, ReferenceType, RevisionHashInput, RevisionPolicy, Slug, Snapshot,
    SnapshotNode, SnapshotWorkspace, SNAPSHOT_FORMAT_VERSION,
};

/// Builds the exact Northstar Platform reference workspace from the specification.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn northstar_platform_snapshot() -> Snapshot {
    let root = id("01JZ8Q5CWPN8T7KPN5A1V9B6XM");
    let architecture = id("01JZ8Q5CWPN8T7KPN5A1V9B6XN");
    let decisions = id("01JZ8Q5CWPN8T7KPN5A1V9B6XP");
    let postgres = id("01JZ8Q5CWPN8T7KPN5A1V9B6XQ");
    let kafka = id("01JZ8Q5CWPN8T7KPN5A1V9B6XR");
    let jwt = id("01JZ8Q5CWPN8T7KPN5A1V9B6XS");
    let services = id("01JZ8Q5CWPN8T7KPN5A1V9B6XT");
    let catalog = id("01JZ8Q5CWPN8T7KPN5A1V9B6XV");
    let orders_service = id("01JZ8Q5CWPN8T7KPN5A1V9B6XW");
    let payments_service = id("01JZ8Q5CWPN8T7KPN5A1V9B6XX");
    let inventory_service = id("01JZ8Q5CWPN8T7KPN5A1V9B6XY");
    let mut decisions_metadata = metadata("Architecture Decisions", "collection");
    decisions_metadata.owns = vec![
        "cross-cutting technical decisions".into(),
        "architectural constraints".into(),
        "decision rationale and consequences".into(),
    ];
    decisions_metadata.accepts_children = vec![node_type("architecture_decision")];
    decisions_metadata.excludes = vec!["service runbooks".into(), "feature plans".into()];
    let mut catalog_metadata = metadata("Service Catalog", "collection");
    catalog_metadata.owns = vec![
        "service responsibilities".into(),
        "service interfaces".into(),
        "operational ownership".into(),
    ];
    catalog_metadata.accepts_children = vec![node_type("service")];
    let nodes = vec![
        node(
            root,
            None,
            "northstar-platform",
            metadata("Northstar Platform", "project"),
            "# Northstar Platform\nArchitecture knowledge for humans and AI coding agents.\n",
            0,
        ),
        node(
            architecture,
            Some(root),
            "architecture",
            metadata("Architecture", "area"),
            "# Architecture\nSystem-wide design, constraints, and decisions.\n",
            0,
        ),
        node(
            decisions,
            Some(architecture),
            "architecture-decisions",
            decisions_metadata,
            "# Architecture Decisions\nCanonical records of consequential technical choices.\n",
            0,
        ),
        node(
            postgres,
            Some(decisions),
            "postgresql-as-system-of-record",
            metadata("ADR-001 — PostgreSQL as System of Record", "architecture_decision"),
            "# ADR-001 — PostgreSQL as System of Record\nTransactional service data is stored in PostgreSQL.\n",
            0,
        ),
        node(
            kafka,
            Some(decisions),
            "domain-events-via-kafka",
            metadata("ADR-002 — Domain Events via Kafka", "architecture_decision"),
            "# ADR-002 — Domain Events via Kafka\nServices publish durable domain events through Kafka.\n",
            1,
        ),
        node(
            jwt,
            Some(decisions),
            "jwt-service-authentication",
            metadata("ADR-003 — JWT Service Authentication", "architecture_decision"),
            "# ADR-003 — JWT Service Authentication\nInternal and public requests use signed JWT credentials.\n",
            2,
        ),
        node(
            services,
            Some(root),
            "services",
            metadata("Services", "area"),
            "# Services\nResponsibilities, interfaces, dependencies, and runbooks.\n",
            1,
        ),
        node(
            catalog,
            Some(services),
            "service-catalog",
            catalog_metadata,
            "# Service Catalog\nCanonical descriptions of deployable services.\n",
            0,
        ),
        node(
            orders_service,
            Some(catalog),
            "orders-service",
            metadata("Orders Service", "service"),
            "# Orders Service\nOwns the order lifecycle and publishes order domain events.\n",
            0,
        ),
        node(
            payments_service,
            Some(catalog),
            "payments-service",
            metadata("Payments Service", "service"),
            "# Payments Service\nProcesses idempotent payments, stores transactions, and publishes payment events.\n",
            1,
        ),
        node(
            inventory_service,
            Some(catalog),
            "inventory-service",
            metadata("Inventory Service", "service"),
            "# Inventory Service\nReserves stock for orders and publishes inventory events.\n",
            2,
        ),
    ];
    let references = vec![
        reference(
            orders_service,
            kafka,
            "ADR-002 — Domain Events via Kafka",
            "implements",
        ),
        reference(
            payments_service,
            postgres,
            "ADR-001 — PostgreSQL as System of Record",
            "implements",
        ),
        reference(
            payments_service,
            kafka,
            "ADR-002 — Domain Events via Kafka",
            "implements",
        ),
        reference(
            inventory_service,
            kafka,
            "ADR-002 — Domain Events via Kafka",
            "implements",
        ),
        reference(jwt, services, "Services", "applies_to"),
    ];
    Snapshot {
        format: "mdtree-snapshot".into(),
        format_version: SNAPSHOT_FORMAT_VERSION,
        workspace: SnapshotWorkspace {
            name: "Northstar Platform".into(),
            workspace_format_version: 1,
        },
        revision_policy: RevisionPolicy::HeadOnly,
        nodes,
        revisions: Vec::new(),
        references,
    }
}

fn id(value: &str) -> NodeId {
    NodeId::from_str(value).expect("fixture ID")
}
fn node_type(value: &str) -> NodeType {
    NodeType::from_str(value).expect("fixture type")
}
fn metadata(title: &str, kind: &str) -> NodeMetadata {
    let mut value = NodeMetadata::new(title);
    value.node_type = Some(node_type(kind));
    value
}

fn node(
    id: NodeId,
    parent_id: Option<NodeId>,
    slug: &str,
    metadata: NodeMetadata,
    markdown: &str,
    sibling_order: u32,
) -> SnapshotNode {
    let slug = Slug::from_str(slug).expect("fixture slug");
    let revision_hash = hash_revision(RevisionHashInput {
        node_id: id,
        parent_id,
        slug: &slug,
        metadata: &metadata,
        markdown_content: markdown,
        sibling_order,
    })
    .expect("fixture hash");
    SnapshotNode {
        id,
        parent_id,
        slug,
        metadata,
        markdown_content: markdown.into(),
        sibling_order,
        version: 1,
        content_hash: hash_content(markdown),
        revision_hash,
        created_at: 1,
        updated_at: 1,
    }
}

fn reference(source_node_id: NodeId, node_id: NodeId, target_ref: &str, kind: &str) -> Reference {
    Reference {
        source_node_id,
        source_section_id: None,
        reference_type: ReferenceType::from_str(kind).expect("fixture relation"),
        target: ReferenceTarget::Resolved {
            node_id,
            target_ref: Some(target_ref.into()),
            anchor: None,
        },
        origin: ReferenceOrigin::Explicit,
        metadata: BTreeMap::<String, Value>::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::northstar_platform_snapshot;
    use crate::validate_snapshot;

    #[test]
    fn canonical_northstar_platform_fixture_is_valid_and_complete() {
        let snapshot = northstar_platform_snapshot();
        assert!(validate_snapshot(&snapshot).is_valid());
        assert_eq!(snapshot.nodes.len(), 11);
        assert_eq!(snapshot.references.len(), 5);
    }
}
