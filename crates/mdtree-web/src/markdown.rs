//! Markdown-to-HTML rendering for the read-only viewer.
//!
//! Rendered Markdown is untrusted workspace content and must be sanitized
//! before insertion into the page: it must not be able to execute scripts,
//! inject application controls, or reach session credentials.

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd, TextMergeStream};

/// Renders `markdown` to sanitized HTML safe to insert directly into the page.
#[must_use]
pub(crate) fn render_sanitized_html(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = TextMergeStream::new(Parser::new_ext(markdown, options));
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, with_highlights(parser));

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

/// Rewrites `==highlighted==` spans inside plain text into `<mark>` elements
/// (the syntax Obsidian and other editors use, which pulldown-cmark has no option for).
///
/// A span must open and close within one run of text, so it can't wrap other
/// inline markup (`==**bold**==` stays literal) — the editor preview
/// (`highlightMarks` in app.js) applies the identical rule so both agree.
/// Code spans and code blocks are never touched, and neither is image alt
/// text (rendered as a plain attribute value, where markup can't go).
fn with_highlights<'a>(events: impl Iterator<Item = Event<'a>>) -> impl Iterator<Item = Event<'a>> {
    let mut in_code_block = false;
    let mut image_depth = 0usize;
    events.flat_map(move |event| {
        match &event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Start(Tag::Image { .. }) => image_depth += 1,
            Event::End(TagEnd::Image) => image_depth = image_depth.saturating_sub(1),
            _ => {}
        }
        match event {
            Event::Text(text) if !in_code_block && image_depth == 0 => split_highlights(text),
            other => vec![other],
        }
    })
}

fn split_highlights(text: CowStr<'_>) -> Vec<Event<'_>> {
    let spans = highlight_spans(&text);
    if spans.is_empty() {
        return vec![Event::Text(text)];
    }
    let mut events = Vec::with_capacity(spans.len() * 4 + 1);
    let mut cursor = 0;
    for (open, close) in spans {
        if open > cursor {
            events.push(Event::Text(text[cursor..open].to_owned().into()));
        }
        events.push(Event::InlineHtml("<mark>".into()));
        events.push(Event::Text(text[open + 2..close].to_owned().into()));
        events.push(Event::InlineHtml("</mark>".into()));
        cursor = close + 2;
    }
    if cursor < text.len() {
        events.push(Event::Text(text[cursor..].to_owned().into()));
    }
    events
}

/// Byte offsets of each paired `==` opener/closer in `text`. A delimiter is
/// exactly two `=` (a longer run like `===` is never one); an opener must be
/// followed by non-whitespace and a closer preceded by it, so ordinary prose
/// such as `a == b` is left alone.
fn highlight_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'=' {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < bytes.len() && bytes[i] == b'=' {
            i += 1;
        }
        if i - run_start != 2 {
            continue;
        }
        let next_is_text = text[i..].chars().next().is_some_and(|c| !c.is_whitespace());
        let prev_is_text = text[..run_start]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace());
        match open {
            Some(start) if prev_is_text && run_start > start + 2 => {
                spans.push((start, run_start));
                open = None;
            }
            None if next_is_text => open = Some(run_start),
            _ => {}
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::render_sanitized_html;

    #[test]
    fn renders_double_equals_as_highlight() {
        let html = render_sanitized_html("some ==a < b & c== here");
        assert!(html.contains("<mark>a &lt; b &amp; c</mark>"), "{html}");
        assert!(html.contains("some ") && html.contains(" here"));
    }

    #[test]
    fn highlights_multiple_spans_and_inside_other_markup() {
        let html = render_sanitized_html("==a== and ==b==\n\n# ==Title==\n\n**bold ==c== bold**");
        assert!(html.contains("<mark>a</mark> and <mark>b</mark>"), "{html}");
        assert!(html.contains("<mark>Title</mark>"), "{html}");
        assert!(
            html.contains("<strong>bold <mark>c</mark> bold</strong>"),
            "{html}"
        );
    }

    #[test]
    fn leaves_non_highlight_equals_alone() {
        for source in [
            "a == b == c",
            "==open only",
            "a ==== b ====",
            "x = y",
            "== ==",
        ] {
            assert!(
                !render_sanitized_html(source).contains("<mark>"),
                "{source}"
            );
        }
    }

    #[test]
    fn never_highlights_inside_code_or_image_alt() {
        let html = render_sanitized_html(
            "`==a==`\n\n```\n==b==\n```\n\n![==c==](https://example.com/p.png)",
        );
        assert!(!html.contains("<mark>"), "{html}");
        assert!(html.contains("==a==") && html.contains("==b==") && html.contains("==c=="));
    }

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
