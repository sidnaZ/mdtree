//! `asset:` image references in node Markdown.

use mdtree_core::AssetUrl;
use pulldown_cmark::{Event, Options, Parser, Tag};

/// Every `asset:` image destination in `markdown`, in document order.
///
/// Uses the Markdown parser, so code spans, fenced code, and ordinary links
/// (as opposed to images) are never mistaken for asset references.
#[must_use]
pub fn extract_asset_references(markdown: &str) -> Vec<AssetUrl> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    Parser::new_ext(markdown, options)
        .filter_map(|event| match event {
            Event::Start(Tag::Image { dest_url, .. }) => AssetUrl::parse(&dest_url),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::extract_asset_references;

    #[test]
    fn only_real_asset_images_are_references() {
        let markdown = "![a](asset:a.png?width=50%)\n\n| x |\n|---|\n| ![t](asset:t.gif) |\n\n\
                        `![c](asset:code.png)`\n\n```\n![f](asset:fence.png)\n```\n\n\
                        [link](asset:link.png) ![remote](https://example.com/r.png)";
        let names: Vec<_> = extract_asset_references(markdown)
            .into_iter()
            .map(|url| (url.name.to_string(), url.width_percent))
            .collect();
        assert_eq!(
            names,
            vec![("a.png".into(), Some(50)), ("t.gif".into(), None)]
        );
    }
}
