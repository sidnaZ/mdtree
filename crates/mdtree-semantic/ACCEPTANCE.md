# Semantic search acceptance evidence

This note records the reproducible evidence behind the initial exact-vector-scan design. It is specific to the local Ollama implementation and does not change lexical search defaults.

## Relevance and compatibility

- `conceptual_synonym_queries_outperform_lexical_token_matching` uses three queries with no shared content tokens—failed-payment retries, card-refusal handling, and payment-error delay strategy. Lexical search returns no match; the deterministic semantic fixture returns the section describing exponential backoff after a declined charge.
- The checked-in `hybrid_relevance.json` cases prove overlap promotion, lexical-only ordering, stable cross-channel ties, normalized scores, and channel-labelled explanations.
- Existing CLI, MCP stdio, browser, migration, backup, rebuild, stale-worker, retry, deletion, rollback, cursor, scope, and filter suites run without a live provider. Lexical remains the default and canonical writes do not call Ollama.

## Privacy and failure behavior

- Provider tests reject credential-bearing URLs, validate model/count/dimensions/finiteness, classify timeout/model/context/provider failures, and assert that malformed responses and errors do not reproduce document content.
- Runtime Ollama configuration remains process state. Workspace tables store profile identity and derived vectors, not endpoint credentials.
- CLI, MCP, and web return stable semantic error categories. Hybrid fallback is opt-in and explicitly labelled.

## Exact-scan scale baseline

Run:

```text
cargo bench -p mdtree-sqlite --bench services -- semantic_exact_scan_3000x384 --sample-size 10 --warm-up-time 0.1 --measurement-time 0.5
```

Observed on 2026-07-28 in the project development container:

```text
chunks=3000
dimensions=384
vector_payload_bytes=4608000
database_bytes=11280384
time=[43.204 ms 43.354 ms 43.518 ms]
```

The benchmark asserts `scanned_chunks == 3000`. Vector blobs therefore contribute exactly 4 bytes × dimensions × ready chunks. Retrieval decodes one vector at a time; transient vector memory is 1,536 bytes at 384 dimensions. Ranking memory is proportional to eligible node IDs and one best result per node, not all decoded vectors.

Exact scan is accepted for the initial implementation at this scale. Create a separate vector-extension/ANN work item when a representative workspace crosses either of these evidence-based boundaries:

- measured local exact-search p95 exceeds 100 ms, or
- eligible ready chunks regularly exceed the scale at which the linear work counter predicts that threshold.

Re-run the checked-in benchmark on target hardware before choosing an extension. Any replacement must preserve profile compatibility, filters, deterministic ties, explanations, semantic revision cursors, and the exact-scan fallback/reference implementation.
