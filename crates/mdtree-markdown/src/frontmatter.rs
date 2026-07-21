//! YAML frontmatter interoperability for Markdown snapshots.

use mdtree_core::NodeMetadata;

use crate::MarkdownError;

/// Parsed snapshot metadata and canonical Markdown body.
#[derive(Clone, Debug, PartialEq)]
pub struct FrontmatterDocument {
    /// Structured metadata when a frontmatter block is present.
    pub metadata: Option<NodeMetadata>,
    /// Markdown after the closing frontmatter delimiter.
    pub body: String,
}

/// Parses an optional leading YAML frontmatter block.
///
/// # Errors
///
/// Returns [`MarkdownError::InvalidFrontmatter`] for a missing closing
/// delimiter or [`MarkdownError::Yaml`] for invalid metadata YAML.
pub fn parse_frontmatter(markdown: &str) -> Result<FrontmatterDocument, MarkdownError> {
    let mut lines = markdown.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Ok(FrontmatterDocument {
            metadata: None,
            body: String::new(),
        });
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Ok(FrontmatterDocument {
            metadata: None,
            body: markdown.into(),
        });
    }

    let yaml_start = first.len();
    let mut offset = yaml_start;
    for line in lines {
        let line_end = offset + line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let metadata = serde_yaml::from_str(&markdown[yaml_start..offset])?;
            return Ok(FrontmatterDocument {
                metadata: Some(metadata),
                body: markdown[line_end..].into(),
            });
        }
        offset = line_end;
    }

    Err(MarkdownError::InvalidFrontmatter(
        "opening delimiter has no closing delimiter".into(),
    ))
}

/// Renders metadata and Markdown as a deterministic snapshot document.
///
/// # Errors
///
/// Returns [`MarkdownError::Yaml`] if metadata serialization fails.
pub fn render_frontmatter(metadata: &NodeMetadata, body: &str) -> Result<String, MarkdownError> {
    let yaml = serde_yaml::to_string(metadata)?;
    Ok(format!("---\n{yaml}---\n{body}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_frontmatter, render_frontmatter};

    #[test]
    fn frontmatter_round_trips_lists_relations_extensions_and_unicode() {
        let source = "---\ntitle: Datu modeļi\naliases:\n  - DB modeļi\nkeywords: [shēma, tabula]\nrelations:\n  - type: depends_on\n    target: API\ncustom_flag: true\n---\n# Datu modeļi\nSaturs.\n";
        let parsed = parse_frontmatter(source).expect("valid frontmatter");
        let metadata = parsed.metadata.expect("metadata block");
        assert_eq!(metadata.aliases, ["DB modeļi"]);
        assert_eq!(metadata.extensions.get("custom_flag"), Some(&json!(true)));
        assert!(metadata.extensions.contains_key("relations"));

        let rendered = render_frontmatter(&metadata, &parsed.body).expect("rendered frontmatter");
        let reparsed = parse_frontmatter(&rendered).expect("round-trip frontmatter");
        assert_eq!(reparsed.metadata, Some(metadata));
        assert_eq!(reparsed.body, "# Datu modeļi\nSaturs.\n");
    }

    #[test]
    fn plain_markdown_and_unclosed_frontmatter_are_distinguished() {
        let plain = parse_frontmatter("# Plain\n").expect("plain Markdown");
        assert_eq!(plain.metadata, None);
        assert!(parse_frontmatter("---\ntitle: Broken\n").is_err());
    }
}
