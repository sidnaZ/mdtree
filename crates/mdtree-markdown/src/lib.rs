//! Markdown parsing and snapshot support for `MDTree`.

mod derived;
mod frontmatter;
mod links;
mod sections;
mod snapshot;

pub use derived::{build_derived_records, DerivedNodeRecords, FtsDocument};
pub use frontmatter::{parse_frontmatter, render_frontmatter, FrontmatterDocument};
pub use links::{extract_markdown_links, extract_wikilinks, LinkKind, MarkdownLink, Wikilink};
pub use sections::{generate_anchor, parse_sections, AnchorRegistry, MarkdownError};
pub use snapshot::{
    export_markdown_snapshot, export_markdown_subtree, parse_markdown_snapshot,
    MarkdownSnapshotError,
};
