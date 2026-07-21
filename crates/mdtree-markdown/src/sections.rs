//! Heading-oriented Markdown section parsing and stable anchors.

use std::collections::HashMap;

use mdtree_core::{hash_content, NodeId, Section, UlidGenerator};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

/// Markdown adapter failure.
#[derive(Debug, Error)]
pub enum MarkdownError {
    /// A byte offset cannot fit the portable persisted representation.
    #[error("Markdown byte offset exceeds u64 range")]
    OffsetOverflow,
    /// YAML frontmatter is invalid.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    /// Frontmatter delimiters are malformed.
    #[error("invalid frontmatter: {0}")]
    InvalidFrontmatter(String),
    /// A derived domain record failed validation.
    #[error(transparent)]
    Domain(#[from] mdtree_core::DomainError),
}

/// Tracks duplicate anchor counts within one Markdown document.
#[derive(Clone, Debug, Default)]
pub struct AnchorRegistry {
    counts: HashMap<String, u32>,
}

/// Generates a deterministic project-specific heading anchor.
///
/// Unicode letters and digits are lowercased and retained. Runs of whitespace
/// or hyphens become one hyphen, punctuation is removed, an empty result uses
/// `section`, and duplicate anchors receive `-1`, `-2`, and so on.
#[must_use]
pub fn generate_anchor(heading: &str, registry: &mut AnchorRegistry) -> String {
    let mut base = String::new();
    let mut separator = false;
    for character in heading.nfkc().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            if separator && !base.is_empty() {
                base.push('-');
            }
            base.push(character);
            separator = false;
        } else if character.is_whitespace() || character == '-' {
            separator = !base.is_empty();
        }
    }
    if base.is_empty() {
        base.push_str("section");
    }

    let count = registry.counts.entry(base.clone()).or_insert(0);
    let anchor = if *count == 0 {
        base
    } else {
        format!("{base}-{count}")
    };
    *count += 1;
    anchor
}

/// Parses Markdown into ordered, nested semantic sections.
///
/// Each section occupies a non-overlapping byte range ending at the next real
/// heading. Heading-like text inside code fences is ignored. Content before
/// the first heading becomes a preamble section.
///
/// # Errors
///
/// Returns [`MarkdownError::OffsetOverflow`] only when an input byte offset
/// cannot fit in `u64`.
pub fn parse_sections(
    node_id: NodeId,
    markdown: &str,
    ids: &dyn UlidGenerator,
) -> Result<Vec<Section>, MarkdownError> {
    let headings = collect_headings(markdown);
    let mut sections = Vec::new();
    let mut stack: Vec<(u8, NodeId)> = Vec::new();
    let mut anchors = AnchorRegistry::default();

    if headings.first().is_none_or(|heading| heading.start > 0) && !markdown.is_empty() {
        let end = headings
            .first()
            .map_or(markdown.len(), |heading| heading.start);
        sections.push(section(
            NodeId::new(ids.generate()),
            node_id,
            None,
            None,
            None,
            None,
            0,
            end,
            &markdown[..end],
            0,
        )?);
    }

    for (index, heading) in headings.iter().enumerate() {
        while stack
            .last()
            .is_some_and(|(level, _)| *level >= heading.level)
        {
            stack.pop();
        }
        let id = NodeId::new(ids.generate());
        let parent = stack.last().map(|(_, id)| *id);
        let end = headings
            .get(index + 1)
            .map_or(markdown.len(), |next| next.start);
        let position = u32::try_from(sections.len()).map_err(|_| MarkdownError::OffsetOverflow)?;
        sections.push(section(
            id,
            node_id,
            parent,
            Some(heading.text.clone()),
            Some(heading.level),
            Some(generate_anchor(&heading.text, &mut anchors)),
            heading.start,
            end,
            &markdown[heading.start..end],
            position,
        )?);
        stack.push((heading.level, id));
    }

    Ok(sections)
}

struct Heading {
    start: usize,
    level: u8,
    text: String,
}

fn collect_headings(markdown: &str) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut active: Option<Heading> = None;
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                active = Some(Heading {
                    start: range.start,
                    level: heading_level(level),
                    text: String::new(),
                });
            }
            Event::Text(text) | Event::Code(text) if active.is_some() => {
                if let Some(heading) = active.as_mut() {
                    heading.text.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(heading) = active.take() {
                    headings.push(heading);
                }
            }
            _ => {}
        }
    }
    headings
}

#[allow(clippy::too_many_arguments)]
fn section(
    id: NodeId,
    node_id: NodeId,
    parent_section_id: Option<NodeId>,
    heading: Option<String>,
    heading_level: Option<u8>,
    anchor: Option<String>,
    start: usize,
    end: usize,
    content: &str,
    position: u32,
) -> Result<Section, MarkdownError> {
    Ok(Section {
        id,
        node_id,
        parent_section_id,
        heading,
        heading_level,
        anchor,
        start_byte: u64::try_from(start).map_err(|_| MarkdownError::OffsetOverflow)?,
        end_byte: u64::try_from(end).map_err(|_| MarkdownError::OffsetOverflow)?,
        content: content.into(),
        content_hash: hash_content(content),
        position,
    })
}

const fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::{generate_anchor, parse_sections, AnchorRegistry};
    use mdtree_core::{NodeId, SequentialUlidGenerator};
    use std::str::FromStr;

    fn node_id() -> NodeId {
        NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("fixture ID")
    }

    #[test]
    fn parses_preamble_nesting_duplicates_unicode_code_fences_and_empty_sections() {
        let markdown = "Intro.\n\n# Overview\n## Café\n## Café\n### Empty\n## Next\n```md\n# Not heading\n```\n";
        let sections = parse_sections(node_id(), markdown, &SequentialUlidGenerator::new(1))
            .expect("valid Markdown");
        assert_eq!(sections.len(), 6);
        assert_eq!(sections[0].heading, None);
        assert_eq!(sections[1].heading.as_deref(), Some("Overview"));
        assert_eq!(sections[2].parent_section_id, Some(sections[1].id));
        assert_eq!(sections[2].anchor.as_deref(), Some("café"));
        assert_eq!(sections[3].anchor.as_deref(), Some("café-1"));
        assert_eq!(sections[4].content, "### Empty\n");
        assert!(sections[5].content.contains("# Not heading"));
    }

    #[test]
    fn anchors_are_deterministic_for_punctuation_unicode_and_duplicates() {
        let mut registry = AnchorRegistry::default();
        let headings = ["Hello, World!", "Žlutý kůň", "---", "Hello World"];
        let anchors: Vec<_> = headings
            .into_iter()
            .map(|heading| generate_anchor(heading, &mut registry))
            .collect();
        assert_eq!(
            anchors,
            ["hello-world", "žlutý-kůň", "section", "hello-world-1"]
        );
    }
}
