//! Runs the labeled Northstar Platform destination-accuracy release evaluation.

use std::str::FromStr;

use mdtree_core::{LocateStatus, NodeId, NodeType};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("northstar.mdtree");
    mdtree_sqlite::import_snapshot_new(&path, &mdtree_core::northstar_platform_snapshot())?;
    let store = mdtree_sqlite::SqliteStore::open(&path)?;
    let decisions = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XP")?;
    let services = NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XV")?;
    let decision = NodeType::from_str("architecture_decision")?;
    let service = NodeType::from_str("service")?;
    let labels = [
        (
            "Add architecture decision for API retries",
            Some(&decision),
            decisions,
            false,
        ),
        (
            "Document our tracing architecture decision",
            Some(&decision),
            decisions,
            false,
        ),
        ("Add notifications service", Some(&service), services, false),
        (
            "Document the identity service",
            Some(&service),
            services,
            false,
        ),
    ];
    let mut top1 = 0_u32;
    let mut top_k = 0_u32;
    let mut ambiguity = 0_u32;
    let mut rows = Vec::new();
    for (query, kind, expected, expected_ambiguous) in labels {
        let result = store.locate_target(query, kind)?;
        let predicted = result.candidates.first().map(|item| item.result.node_id);
        top1 += u32::from(predicted == Some(expected));
        top_k += u32::from(
            result
                .candidates
                .iter()
                .any(|item| item.result.node_id == expected),
        );
        let actual_ambiguous = result.status == LocateStatus::Ambiguous;
        ambiguity += u32::from(actual_ambiguous == expected_ambiguous);
        rows.push(serde_json::json!({"query":query,"expected":expected,"predicted":predicted,"status":result.status}));
    }
    let total = u32::try_from(rows.len())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "labels":rows,"top1_correct":top1,"top_k_correct":top_k,
            "ambiguity_correct":ambiguity,"total":total,
            "top1_accuracy":f64::from(top1)/f64::from(total),
            "top_k_accuracy":f64::from(top_k)/f64::from(total),
            "ambiguity_accuracy":f64::from(ambiguity)/f64::from(total)
        }))?
    );
    if top1 != total || ambiguity != total {
        return Err("destination baseline regressed".into());
    }
    Ok(())
}
