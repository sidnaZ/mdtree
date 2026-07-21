//! Markdown-to-HTML rendering for the read-only viewer.
//!
//! Rendered Markdown is untrusted workspace content and must be sanitized
//! before insertion into the page: it must not be able to execute scripts,
//! inject application controls, or reach session credentials.

use pulldown_cmark::{html, Options, Parser};

/// Renders `markdown` to sanitized HTML safe to insert directly into the page.
#[must_use]
pub(crate) fn render_sanitized_html(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(markdown, options);
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, parser);

    ammonia::Builder::default()
        .add_tags(["input"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_generic_attributes(["id"])
        // External links never navigate the embedded browsing context or
        // leak it to the target page.
        .link_rel(Some("noopener noreferrer nofollow"))
        .clean(&unsafe_html)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::render_sanitized_html;

    #[test]
    fn strips_script_tags_and_inline_event_handlers() {
        let html = render_sanitized_html(
            "before\n\n<script>alert('x')</script>\n\n<img src=x onerror=\"alert('y')\">",
        );
        assert!(!html.contains("<script"));
        assert!(!html.contains("onerror"));
        assert!(html.contains("before"));
    }

    #[test]
    fn strips_javascript_urls_from_links() {
        let html = render_sanitized_html("[click me](javascript:alert('x'))");
        assert!(!html.to_lowercase().contains("javascript:"));
    }

    #[test]
    fn preserves_task_list_checkboxes_and_tables() {
        let html =
            render_sanitized_html("- [x] done\n- [ ] todo\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("type=\"checkbox\""));
        assert!(html.contains("checked"));
        assert!(html.contains("<table"));
    }

    #[test]
    fn renders_full_github_style_fidelity() {
        let html = render_sanitized_html(
            "# Heading 1\n\n## Heading 2\n\n> a blockquote\n\n\
             * a list item\n* another\n\n1. first\n2. second\n\n\
             an inline `code span` here.\n\n\
             ```rust\nfn main() {}\n```\n\n\
             ![alt text](https://example.com/pic.png)\n\n\
             [a link](https://example.com)\n",
        );
        assert!(html.contains("<h1"));
        assert!(html.contains("<h2"));
        assert!(html.contains("<blockquote"));
        assert!(html.contains("<ul"));
        assert!(html.contains("<ol"));
        assert!(html.contains("<code>code span</code>"));
        assert!(html.contains("<pre"));
        assert!(html.contains("fn main"));
        assert!(html.contains("<img") && html.contains("example.com/pic.png"));
        assert!(html.contains("<a") && html.contains("href=\"https://example.com\""));
    }

    #[test]
    fn external_links_get_a_safe_rel_attribute() {
        let html = render_sanitized_html("[a link](https://example.com)");
        assert!(html.contains("rel=\"noopener noreferrer nofollow\""));
    }
}
