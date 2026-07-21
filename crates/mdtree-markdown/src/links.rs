//! Standard Markdown link and wikilink extraction.

use mdtree_core::{NodeId, Section};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// Broad destination category for a standard Markdown link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkKind {
    /// Relative path, absolute local path, or fragment.
    Internal,
    /// URI with an explicit scheme or protocol-relative destination.
    External,
}

/// Extracted standard Markdown link.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkdownLink {
    /// Destination exactly as parsed by the Markdown parser.
    pub destination: String,
    /// Rendered textual label.
    pub label: String,
    /// Internal or external destination category.
    pub kind: LinkKind,
    /// Semantic source section, when ranges are available.
    pub source_section_id: Option<NodeId>,
}

/// Extracted `[[target|label]]` relationship.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Wikilink {
    /// Target text before an optional anchor.
    pub target: String,
    /// Optional display label after `|`.
    pub label: Option<String>,
    /// Optional target anchor after `#`.
    pub anchor: Option<String>,
    /// Semantic source section, when ranges are available.
    pub source_section_id: Option<NodeId>,
}

/// Extracts standard Markdown links, excluding images.
#[must_use]
pub fn extract_markdown_links(markdown: &str, sections: &[Section]) -> Vec<MarkdownLink> {
    let mut links = Vec::new();
    let mut active: Option<(String, String, usize)> = None;
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                active = Some((dest_url.into_string(), String::new(), range.start));
            }
            Event::Text(text) | Event::Code(text) if active.is_some() => {
                if let Some((_, label, _)) = active.as_mut() {
                    label.push_str(&text);
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some((destination, label, start)) = active.take() {
                    links.push(MarkdownLink {
                        kind: classify(&destination),
                        destination,
                        label,
                        source_section_id: section_at(sections, start),
                    });
                }
            }
            _ => {}
        }
    }
    links
}

/// Extracts valid wikilinks while ignoring escapes, code spans, and code fences.
#[must_use]
pub fn extract_wikilinks(markdown: &str, sections: &[Section]) -> Vec<Wikilink> {
    let excluded = code_bytes(markdown);
    let bytes = markdown.as_bytes();
    let mut links = Vec::new();
    let mut cursor = 0;

    while cursor + 1 < bytes.len() {
        if bytes[cursor] != b'['
            || bytes[cursor + 1] != b'['
            || excluded[cursor]
            || is_escaped(bytes, cursor)
        {
            cursor += 1;
            continue;
        }
        let Some(relative_end) = markdown[cursor + 2..].find("]]") else {
            break;
        };
        let end = cursor + 2 + relative_end;
        if excluded.get(end).copied().unwrap_or(false) {
            cursor = end + 2;
            continue;
        }
        if let Some(link) = parse_wikilink(&markdown[cursor + 2..end], sections, cursor) {
            links.push(link);
        }
        cursor = end + 2;
    }
    links
}

fn parse_wikilink(inner: &str, sections: &[Section], offset: usize) -> Option<Wikilink> {
    if inner.contains(['[', ']', '\n', '\r']) {
        return None;
    }
    let (target_with_anchor, label) = inner
        .split_once('|')
        .map_or((inner, None), |(target, label)| {
            (target, Some(label.trim().to_owned()))
        });
    let (target, anchor) = target_with_anchor
        .split_once('#')
        .map_or((target_with_anchor, None), |(target, anchor)| {
            (target, Some(anchor.trim().to_owned()))
        });
    let target = target.trim();
    if target.is_empty()
        || label.as_ref().is_some_and(String::is_empty)
        || anchor.as_ref().is_some_and(String::is_empty)
    {
        return None;
    }
    Some(Wikilink {
        target: target.into(),
        label,
        anchor,
        source_section_id: section_at(sections, offset),
    })
}

fn classify(destination: &str) -> LinkKind {
    if destination.starts_with("//")
        || destination.split_once(':').is_some_and(|(scheme, _)| {
            !scheme.is_empty() && scheme.chars().all(char::is_alphanumeric)
        })
    {
        LinkKind::External
    } else {
        LinkKind::Internal
    }
}

fn section_at(sections: &[Section], offset: usize) -> Option<NodeId> {
    let offset = u64::try_from(offset).ok()?;
    sections
        .iter()
        .find(|section| section.start_byte <= offset && offset < section.end_byte)
        .map(|section| section.id)
}

fn is_escaped(bytes: &[u8], offset: usize) -> bool {
    let slash_count = bytes[..offset]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count();
    slash_count % 2 == 1
}

fn code_bytes(markdown: &str) -> Vec<bool> {
    let mut excluded = vec![false; markdown.len()];
    let mut in_block = false;
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        if matches!(event, Event::Start(Tag::CodeBlock(_))) {
            in_block = true;
        }
        if in_block || matches!(event, Event::Code(_)) {
            for byte in &mut excluded[range] {
                *byte = true;
            }
        }
        if matches!(event, Event::End(TagEnd::CodeBlock)) {
            in_block = false;
        }
    }
    excluded
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mdtree_core::{NodeId, SequentialUlidGenerator};

    use super::{extract_markdown_links, extract_wikilinks, LinkKind};
    use crate::parse_sections;

    fn fixture(markdown: &str) -> Vec<mdtree_core::Section> {
        parse_sections(
            NodeId::from_str("01JZ8Q5CWPN8T7KPN5A1V9B6XM").expect("fixture ID"),
            markdown,
            &SequentialUlidGenerator::new(20),
        )
        .expect("sections")
    }

    #[test]
    fn standard_links_cover_relative_fragments_escapes_and_images() {
        let markdown = "# Links\n[relative](../node.md#part) [fragment](#part) [web](https://example.com) \\[escaped](no.md) ![image](pic.png)";
        let links = extract_markdown_links(markdown, &fixture(markdown));
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].destination, "../node.md#part");
        assert_eq!(links[0].kind, LinkKind::Internal);
        assert_eq!(links[1].destination, "#part");
        assert_eq!(links[2].kind, LinkKind::External);
        assert!(links.iter().all(|link| link.source_section_id.is_some()));
    }

    #[test]
    fn wikilinks_cover_labels_anchors_repeats_malformed_escapes_and_code() {
        let markdown = "# Links\n[[Target]] [[Target#part|Label]] [[Target]] \\[[Escaped]] [[|bad]] `[[Code]]`\n```\n[[Fence]]\n```";
        let links = extract_wikilinks(markdown, &fixture(markdown));
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].target, "Target");
        assert_eq!(links[1].anchor.as_deref(), Some("part"));
        assert_eq!(links[1].label.as_deref(), Some("Label"));
        assert_eq!(links[2], links[0]);
    }
}
