//! Deterministic Markdown-section inputs for embedding providers.

use mdtree_core::{
    hash_embedding_input, EmbeddingProfile, Node, Section, SemanticChunk,
    SEMANTIC_INPUT_FORMAT_VERSION,
};

use crate::MarkdownError;

/// Conservative default maximum for one exact formatted embedding input.
pub const DEFAULT_SEMANTIC_INPUT_MAX_BYTES: usize = 8 * 1024;
/// Default repeated source-content budget between adjacent chunks.
pub const DEFAULT_SEMANTIC_OVERLAP_BYTES: usize = 512;

/// Deterministic byte bounds used to derive embedding inputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticChunkOptions {
    /// Maximum UTF-8 bytes in the complete formatted provider input.
    pub max_input_bytes: usize,
    /// Desired repeated source-content bytes between adjacent chunks.
    pub overlap_bytes: usize,
}

impl Default for SemanticChunkOptions {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_SEMANTIC_INPUT_MAX_BYTES,
            overlap_bytes: DEFAULT_SEMANTIC_OVERLAP_BYTES,
        }
    }
}

/// Builds bounded embedding inputs from already parsed Markdown sections.
///
/// The exact input contains node metadata, the section heading, and one
/// contiguous source-content range. Ancestor and child context are excluded so
/// unrelated tree mutations do not invalidate a subtree's embeddings.
///
/// # Errors
///
/// Returns [`MarkdownError`] when the requested input format is unsupported,
/// metadata leaves no room for content, options cannot guarantee progress, or
/// portable offsets and positions overflow.
pub fn build_semantic_chunks(
    node: &Node,
    sections: &[Section],
    profile: &EmbeddingProfile,
    options: SemanticChunkOptions,
) -> Result<Vec<SemanticChunk>, MarkdownError> {
    if profile.input_format_version != SEMANTIC_INPUT_FORMAT_VERSION {
        return Err(MarkdownError::UnsupportedSemanticInputFormat(
            profile.input_format_version,
        ));
    }

    let mut chunks = Vec::new();
    for section in sections {
        if section.node_id != node.id() {
            return Err(MarkdownError::SemanticSectionNodeMismatch {
                expected: node.id(),
                actual: section.node_id,
            });
        }
        let prefix = format_prefix(node, section);
        let minimum_content = section.content.chars().next().map_or(0, char::len_utf8);
        if prefix.len().saturating_add(minimum_content) > options.max_input_bytes {
            return Err(MarkdownError::SemanticInputLimit {
                required: prefix.len().saturating_add(minimum_content),
                maximum: options.max_input_bytes,
            });
        }
        let content_budget = options.max_input_bytes - prefix.len();
        if options.overlap_bytes >= content_budget / 2 && section.content.len() > content_budget {
            return Err(MarkdownError::SemanticChunkOptions(
                "overlap_bytes must be less than half the available content budget".into(),
            ));
        }
        append_section_chunks(
            &mut chunks,
            section,
            profile,
            &prefix,
            content_budget,
            options.overlap_bytes,
        )?;
    }
    Ok(chunks)
}

fn format_prefix(node: &Node, section: &Section) -> String {
    let metadata = &node.fields().metadata;
    format!(
        "mdtree_semantic_input_v{SEMANTIC_INPUT_FORMAT_VERSION}\n\
         title={}\n\
         summary={}\n\
         aliases={}\n\
         tags={}\n\
         keywords={}\n\
         heading={}\n\
         content=\n",
        json(&metadata.title),
        json(&metadata.summary),
        json(&metadata.aliases),
        json(&metadata.tags),
        json(&metadata.keywords),
        json(&section.heading),
    )
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("strings and string collections always serialize")
}

fn append_section_chunks(
    chunks: &mut Vec<SemanticChunk>,
    section: &Section,
    profile: &EmbeddingProfile,
    prefix: &str,
    content_budget: usize,
    overlap_bytes: usize,
) -> Result<(), MarkdownError> {
    let content = section.content.as_str();
    let mut start = 0;
    let mut position = 0_u32;
    loop {
        let end = chunk_end(content, start, content_budget);
        if end == start && start < content.len() {
            let next_character_bytes = content[start..].chars().next().map_or(0, char::len_utf8);
            return Err(MarkdownError::SemanticInputLimit {
                required: prefix.len().saturating_add(next_character_bytes),
                maximum: prefix.len().saturating_add(content_budget),
            });
        }
        let input = prefix.to_owned() + &content[start..end];
        debug_assert!(input.len() <= prefix.len() + content_budget);
        let start_byte = section
            .start_byte
            .checked_add(u64::try_from(start).map_err(|_| MarkdownError::OffsetOverflow)?)
            .ok_or(MarkdownError::OffsetOverflow)?;
        let end_byte = section
            .start_byte
            .checked_add(u64::try_from(end).map_err(|_| MarkdownError::OffsetOverflow)?)
            .ok_or(MarkdownError::OffsetOverflow)?;
        chunks.push(SemanticChunk {
            node_id: section.node_id,
            section_id: section.id,
            position,
            start_byte,
            end_byte,
            input_hash: hash_embedding_input(profile, &input),
            input,
        });
        if end == content.len() {
            break;
        }
        start = next_start(content, start, end, overlap_bytes);
        position = position
            .checked_add(1)
            .ok_or(MarkdownError::OffsetOverflow)?;
    }
    Ok(())
}

fn chunk_end(content: &str, start: usize, budget: usize) -> usize {
    let maximum = floor_char_boundary(content, start.saturating_add(budget).min(content.len()));
    if maximum == content.len() {
        return maximum;
    }
    let minimum = floor_char_boundary(content, start + (maximum - start) * 3 / 4);
    let candidate = &content[minimum..maximum];
    for separator in ["\n\n", "\n"] {
        if let Some(relative) = candidate.rfind(separator) {
            return minimum + relative + separator.len();
        }
    }
    if let Some((relative, character)) = candidate
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
    {
        return minimum + relative + character.len_utf8();
    }
    maximum
}

fn next_start(content: &str, previous: usize, end: usize, overlap: usize) -> usize {
    let candidate = floor_char_boundary(content, end.saturating_sub(overlap));
    if candidate > previous {
        candidate
    } else {
        end
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{
        EmbeddingMetric, EmbeddingProfile, Node, NodeFields, NodeHash, NodeId, NodeMetadata,
        SequentialUlidGenerator, Slug, SEMANTIC_INPUT_FORMAT_VERSION,
    };

    use crate::{parse_sections, MarkdownError};

    use super::{build_semantic_chunks, SemanticChunkOptions};

    fn node_with_title(markdown: &str, title: &str) -> Node {
        let mut metadata = NodeMetadata::new(title);
        metadata.summary = Some("Charging and refunds".into());
        metadata.aliases = vec!["Billing".into()];
        metadata.tags = vec!["finance".into()];
        metadata.keywords = vec!["charge".into(), "refund".into()];
        Node::new(
            NodeFields {
                id: NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("ID"),
                slug: Slug::from_str("payments").expect("slug"),
                metadata,
                markdown_content: markdown.into(),
                sibling_order: 0,
                version: 1,
                content_hash: NodeHash::new([0; 32]),
                revision_hash: NodeHash::new([0; 32]),
                created_at: 1,
                updated_at: 1,
            },
            None,
        )
        .expect("node")
    }

    fn node(markdown: &str) -> Node {
        node_with_title(markdown, "Payments")
    }

    fn profile() -> EmbeddingProfile {
        EmbeddingProfile {
            provider: "ollama".into(),
            model: "embeddinggemma".into(),
            dimensions: 768,
            metric: EmbeddingMetric::Cosine,
            input_format_version: SEMANTIC_INPUT_FORMAT_VERSION,
        }
    }

    fn chunks(node: &Node, maximum: usize, overlap: usize) -> Vec<mdtree_core::SemanticChunk> {
        let sections = parse_sections(
            node.id(),
            &node.fields().markdown_content,
            &SequentialUlidGenerator::new(1),
        )
        .expect("sections");
        build_semantic_chunks(
            node,
            &sections,
            &profile(),
            SemanticChunkOptions {
                max_input_bytes: maximum,
                overlap_bytes: overlap,
            },
        )
        .expect("chunks")
    }

    #[test]
    fn exact_input_is_stable_and_contains_only_document_context() {
        let node = node("# Retry policy\nUse exponential backoff.");
        let first = chunks(&node, 1024, 32);
        let second = chunks(&node, 1024, 32);
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert!(first[0].input.contains("title=\"Payments\""));
        assert!(first[0].input.contains("summary=\"Charging and refunds\""));
        assert!(first[0].input.contains("aliases=[\"Billing\"]"));
        assert!(first[0].input.contains("heading=\"Retry policy\""));
        assert!(first[0]
            .input
            .ends_with("# Retry policy\nUse exponential backoff."));
        assert_eq!(first[0].start_byte, 0);
        assert_eq!(
            first[0].end_byte,
            u64::try_from(node.fields().markdown_content.len()).expect("length")
        );
    }

    #[test]
    fn long_unicode_content_is_bounded_overlapping_and_offset_exact() {
        let markdown = format!(
            "# Unicode\n{}\n{}",
            "žluťoučký ".repeat(40),
            "final".repeat(30)
        );
        let node = node(&markdown);
        let chunks = chunks(&node, 360, 24);
        assert!(chunks.len() > 1);
        for (position, chunk) in chunks.iter().enumerate() {
            assert!(chunk.input.len() <= 360);
            assert_eq!(chunk.position, u32::try_from(position).expect("position"));
            let start = usize::try_from(chunk.start_byte).expect("start");
            let end = usize::try_from(chunk.end_byte).expect("end");
            assert!(markdown.is_char_boundary(start));
            assert!(markdown.is_char_boundary(end));
            assert!(chunk.input.ends_with(&markdown[start..end]));
        }
        for pair in chunks.windows(2) {
            assert!(pair[1].start_byte < pair[0].end_byte);
            assert!(pair[1].start_byte > pair[0].start_byte);
        }
    }

    #[test]
    fn exact_limit_and_one_byte_over_have_deterministic_boundaries() {
        let base = node("# A\nshort");
        let one = chunks(&base, 512, 16);
        let exact = one[0].input.len();
        assert_eq!(chunks(&base, exact, 0).len(), 1);

        let longer = node("# A\nshort!");
        let split = chunks(&longer, exact, 0);
        assert_eq!(split.len(), 2);
        assert!(split.iter().all(|chunk| chunk.input.len() <= exact));
    }

    #[test]
    fn metadata_profile_and_format_changes_invalidate_hashes() {
        let original = node("# A\nbody");
        let sections = parse_sections(
            original.id(),
            &original.fields().markdown_content,
            &SequentialUlidGenerator::new(1),
        )
        .expect("sections");
        let options = SemanticChunkOptions {
            max_input_bytes: 1024,
            overlap_bytes: 16,
        };
        let baseline = build_semantic_chunks(&original, &sections, &profile(), options)
            .expect("baseline")[0]
            .input_hash;

        let changed = node_with_title("# A\nbody", "Billing");
        let changed_hash = build_semantic_chunks(&changed, &sections, &profile(), options)
            .expect("changed")[0]
            .input_hash;
        assert_ne!(baseline, changed_hash);
        assert_ne!(
            baseline,
            build_semantic_chunks(
                &original,
                &sections,
                &EmbeddingProfile {
                    model: "all-minilm".into(),
                    ..profile()
                },
                options
            )
            .expect("other profile")[0]
                .input_hash
        );
        assert!(matches!(
            build_semantic_chunks(
                &original,
                &sections,
                &EmbeddingProfile {
                    input_format_version: 2,
                    ..profile()
                },
                options
            ),
            Err(MarkdownError::UnsupportedSemanticInputFormat(2))
        ));
    }

    #[test]
    fn metadata_that_consumes_the_limit_fails_instead_of_truncating() {
        let node = node_with_title("# A\nbody", &"x".repeat(500));
        let sections = parse_sections(
            node.id(),
            &node.fields().markdown_content,
            &SequentialUlidGenerator::new(1),
        )
        .expect("sections");
        assert!(matches!(
            build_semantic_chunks(
                &node,
                &sections,
                &profile(),
                SemanticChunkOptions {
                    max_input_bytes: 128,
                    overlap_bytes: 0
                }
            ),
            Err(MarkdownError::SemanticInputLimit { .. })
        ));
    }

    #[test]
    fn a_budget_smaller_than_the_next_unicode_scalar_fails_without_looping() {
        let node = node("é");
        let sections = parse_sections(
            node.id(),
            &node.fields().markdown_content,
            &SequentialUlidGenerator::new(1),
        )
        .expect("sections");
        let generous = build_semantic_chunks(
            &node,
            &sections,
            &profile(),
            SemanticChunkOptions {
                max_input_bytes: 1024,
                overlap_bytes: 0,
            },
        )
        .expect("baseline");
        let prefix_bytes = generous[0].input.len() - "é".len();
        assert!(matches!(
            build_semantic_chunks(
                &node,
                &sections,
                &profile(),
                SemanticChunkOptions {
                    max_input_bytes: prefix_bytes + 1,
                    overlap_bytes: 0
                }
            ),
            Err(MarkdownError::SemanticInputLimit { .. })
        ));
    }

    #[test]
    fn empty_document_has_no_artificial_chunks() {
        let node = node("");
        assert!(chunks(&node, 512, 16).is_empty());
    }
}
