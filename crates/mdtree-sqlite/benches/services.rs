//! Reproducible service-level Criterion benchmarks.

#![allow(missing_docs)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mdtree_core::{
    hash_content, hash_embedding_input, hash_revision, EmbeddingMetric, EmbeddingProfile, Node,
    NodeFields, NodeRevision, PageLimit, RevisionHashInput, SearchFilters, SearchMode,
    SearchRequest, SearchScope, SemanticChunk, SequentialUlidGenerator,
    SEMANTIC_INPUT_FORMAT_VERSION,
};
use mdtree_sqlite::{import_snapshot_new, NodeChange, SqliteStore};
use tempfile::{tempdir, TempDir};

fn workspace(count: usize) -> (TempDir, SqliteStore) {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("benchmark.mdtree");
    import_snapshot_new(&path, &mdtree_core::generate_benchmark_snapshot(count, 42))
        .expect("import");
    (directory, SqliteStore::open(&path).expect("workspace"))
}

fn semantic_workspace(count: usize, dimensions: u32) -> (TempDir, SqliteStore, EmbeddingProfile) {
    let (directory, mut store) = workspace(count);
    let profile = EmbeddingProfile {
        provider: "benchmark".into(),
        model: "deterministic".into(),
        dimensions,
        metric: EmbeddingMetric::Cosine,
        input_format_version: SEMANTIC_INPUT_FORMAT_VERSION,
    };
    for source in store.semantic_sources().expect("semantic sources") {
        let chunks = source
            .sections
            .iter()
            .map(|section| {
                let input = format!(
                    "semantic benchmark {} {}",
                    source.node.fields().metadata.title,
                    section.position
                );
                SemanticChunk {
                    node_id: source.node.id(),
                    section_id: section.id,
                    position: 0,
                    start_byte: section.start_byte,
                    end_byte: section.end_byte,
                    input_hash: hash_embedding_input(&profile, &input),
                    input,
                }
            })
            .collect::<Vec<_>>();
        store
            .replace_node_semantic_chunks(source.node.id(), &profile, &chunks, 1)
            .expect("semantic chunks");
    }
    let component =
        1.0 / f32::from(u16::try_from(dimensions).expect("benchmark dimensions")).sqrt();
    let vector = vec![component; usize::try_from(dimensions).expect("benchmark dimensions")];
    loop {
        let work = store.claim_semantic_chunks(100, 2).expect("claim");
        if work.is_empty() {
            break;
        }
        for item in work {
            store
                .store_semantic_embedding(&item, &vector, 3)
                .expect("embedding");
        }
    }
    let coverage = store
        .semantic_index_status()
        .expect("semantic status")
        .coverage;
    assert!(coverage.ready >= u64::try_from(count).expect("benchmark count"));
    store
        .connection()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    let database_bytes = std::fs::metadata(directory.path().join("benchmark.mdtree"))
        .expect("database metadata")
        .len();
    eprintln!(
        "semantic scale fixture: chunks={}, dimensions={}, vector_payload_bytes={}, database_bytes={database_bytes}",
        coverage.ready,
        dimensions,
        u64::from(dimensions).saturating_mul(4).saturating_mul(coverage.ready),
    );
    (directory, store, profile)
}

fn bench_services(criterion: &mut Criterion) {
    let (directory, store) = workspace(10_000);
    let path = directory.path().join("benchmark.mdtree");
    criterion.bench_function("open_10000", |bencher| {
        bencher.iter(|| black_box(SqliteStore::open(&path).expect("open")));
    });
    let root = store.root().expect("root").id();
    let subtree_node = store.children(root).expect("children")[0].id();
    criterion.bench_function("traversal_subtree_10000", |bencher| {
        bencher.iter(|| black_box(store.subtree(root).expect("subtree")));
    });
    let workspace_search = SearchRequest {
        query: "searchable benchmark".into(),
        mode: mdtree_core::SearchMode::Lexical,
        scope: SearchScope::Workspace,
        scope_node: None,
        filters: SearchFilters::default(),
        limit: 20,
        offset: 0,
        prefix_last_token: true,
    };
    criterion.bench_function("search_workspace_10000", |bencher| {
        bencher.iter(|| black_box(store.search_content(&workspace_search).expect("search")));
    });
    let subtree_search = SearchRequest {
        scope: SearchScope::Subtree,
        scope_node: Some(subtree_node),
        ..workspace_search.clone()
    };
    criterion.bench_function("search_subtree_10000", |bencher| {
        bencher.iter(|| black_box(store.search_content(&subtree_search).expect("search")));
    });
    let (_semantic_directory, semantic_store, semantic_profile) = semantic_workspace(1_000, 384);
    let semantic_request = SearchRequest {
        query: "conceptual benchmark".into(),
        mode: SearchMode::Semantic,
        scope: SearchScope::Workspace,
        scope_node: None,
        filters: SearchFilters::default(),
        limit: 20,
        offset: 0,
        prefix_last_token: true,
    };
    let semantic_component = 1.0 / 384.0_f32.sqrt();
    let semantic_query = vec![semantic_component; 384];
    let evidence = semantic_store
        .search_semantic_page(
            &semantic_request,
            &semantic_profile,
            &semantic_query,
            PageLimit::new(20).expect("limit"),
            None,
        )
        .expect("semantic evidence");
    assert_eq!(evidence.scanned_chunks, 3_000);
    criterion.bench_function("semantic_exact_scan_3000x384", |bencher| {
        bencher.iter(|| {
            black_box(
                semantic_store
                    .search_semantic_page(
                        &semantic_request,
                        &semantic_profile,
                        &semantic_query,
                        PageLimit::new(20).expect("limit"),
                        None,
                    )
                    .expect("semantic search"),
            )
        });
    });
    criterion.bench_function("locate_destination_10000", |bencher| {
        bencher.iter(|| {
            black_box(
                store
                    .locate_target("add benchmark document", None)
                    .expect("locate"),
            )
        });
    });
    criterion.bench_function("read_context_10000", |bencher| {
        bencher.iter(|| black_box(store.read_context(subtree_node, 16_384).expect("context")));
    });
    criterion.bench_function("rebuild_derived_1000", |bencher| {
        bencher.iter_batched(
            || workspace(1_000),
            |(_directory, mut store)| {
                store
                    .rebuild_derived(&SequentialUlidGenerator::new(1))
                    .expect("rebuild");
            },
            BatchSize::LargeInput,
        );
    });
    criterion.bench_function("update_node_1000", |bencher| {
        bencher.iter_batched(
            || workspace(1_000),
            |(_directory, mut store)| update_root(&mut store),
            BatchSize::LargeInput,
        );
    });
}

fn update_root(store: &mut SqliteStore) {
    let current = store.root().expect("root");
    let f = current.fields();
    let markdown = format!("{}\nupdated", f.markdown_content);
    let revision_hash = hash_revision(RevisionHashInput {
        node_id: current.id(),
        parent_id: None,
        slug: &f.slug,
        metadata: &f.metadata,
        markdown_content: &markdown,
        sibling_order: 0,
    })
    .expect("hash");
    let node = Node::new(
        NodeFields {
            id: current.id(),
            slug: f.slug.clone(),
            metadata: f.metadata.clone(),
            markdown_content: markdown.clone(),
            sibling_order: 0,
            version: 2,
            content_hash: hash_content(&markdown),
            revision_hash,
            created_at: f.created_at,
            updated_at: 2,
        },
        None,
    )
    .expect("node");
    let revision = NodeRevision {
        node_id: node.id(),
        parent_id: None,
        slug: node.fields().slug.clone(),
        metadata: node.fields().metadata.clone(),
        markdown_content: markdown,
        sibling_order: 0,
        version: 2,
        content_hash: node.fields().content_hash,
        revision_hash,
        change_summary: Some("benchmark".into()),
        created_by: Some("benchmark".into()),
        created_at: 2,
    };
    let derived = mdtree_markdown::build_derived_records(&node, &SequentialUlidGenerator::new(2))
        .expect("derived");
    store
        .update_node(NodeChange {
            node: &node,
            expected_version: 1,
            revision: &revision,
            derived: &derived,
        })
        .expect("update");
}

criterion_group!(benches, bench_services);
criterion_main!(benches);
