//! Minimal Office Open XML (`.docx`) writer for the browse UI's "Export
//! DOCX" action. Every exported node becomes a heading paragraph at its tree depth
//! followed by its Markdown body rendered as Word paragraphs and tables. Content is
//! untrusted text: it is XML-escaped and never interpreted as markup.
//!
//! When the export root carries `toc-depth` metadata greater than zero, the
//! document becomes a table-of-contents layout: the root's own content on
//! page one, the table of contents on its own page, and every first-level
//! child starting on a new page. The root's `toc-title` metadata, when set,
//! replaces the default table-of-contents heading, its `fonts` metadata
//! overrides fonts per role (see `docx_fonts`), and descendants with
//! `docx-exclude` metadata are left out together with their subtrees.

use std::fmt::Write as _;

use mdtree_core::{NodeId, NodeMetadata};
use mdtree_sqlite::{NodeDepth, SqliteStore, StoreError};
use serde_json::Value;

use crate::docx_fonts::{CharWidths, Fonts};

/// A generated Word document and how many nodes it contains.
#[derive(Clone, Debug)]
pub struct DocxExport {
    /// Complete `.docx` file contents.
    pub bytes: Vec<u8>,
    /// Exported node count after `docx-exclude` filtering, root included.
    pub nodes: usize,
    /// Title of the exported root node.
    pub root_title: String,
}

/// Exports `root` and its subtree as a Word document: one heading-led
/// section per node, an optional table of contents (`toc-depth` /
/// `toc-title` on `root`), `docx-exclude` subtrees omitted, and page numbers
/// in the footer. Shared by `mdtree export-docx` and the browse UI.
///
/// # Errors
///
/// Returns the store's error when the subtree cannot be read.
pub fn subtree_docx(store: &SqliteStore, root: NodeId) -> Result<DocxExport, StoreError> {
    Ok(docx_from_subtree(store.subtree(root)?))
}

/// Builds the document from an already-read subtree, so a caller holding a
/// lock can release it before the (CPU-only) document build.
pub(crate) fn docx_from_subtree(subtree: Vec<NodeDepth>) -> DocxExport {
    let nodes = export_nodes(subtree);
    DocxExport {
        bytes: build_docx(&nodes),
        nodes: nodes.len(),
        root_title: nodes
            .first()
            .map_or_else(String::new, |node| node.title.to_string()),
    }
}

/// One node of a depth-first subtree export. `depth` is relative to the
/// selected export root, which itself has depth zero.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExportNode {
    pub(crate) depth: u32,
    pub(crate) title: Box<str>,
    pub(crate) markdown: Box<str>,
    /// The node's `toc-depth` metadata: how many levels below this node a
    /// table of contents lists when this node is the export root.
    pub(crate) toc_depth: Option<u32>,
    /// The node's `toc-title` metadata: the table of contents' heading when
    /// this node is the export root.
    pub(crate) toc_title: Option<Box<str>>,
    /// The export root's `fonts` metadata; `None` on every other node.
    pub(crate) fonts: Option<Fonts>,
}

/// Longest accepted `toc-title`, in characters.
const MAX_TOC_TITLE_CHARS: usize = 200;

/// Converts a depth-first subtree (root first) into export nodes, omitting
/// every descendant marked `docx-exclude` together with its own subtree.
/// The selected root itself is always exported.
pub(crate) fn export_nodes(subtree: Vec<NodeDepth>) -> Vec<ExportNode> {
    let mut exclusions = ExcludedSubtrees::default();
    subtree
        .into_iter()
        .filter_map(|entry| {
            let fields = entry.node.fields();
            let metadata = &fields.metadata;
            if !exclusions.keep(
                entry.depth,
                docx_excluded(extension(metadata, "docx-exclude")),
            ) {
                return None;
            }
            Some(ExportNode {
                depth: entry.depth,
                title: metadata.title.clone().into_boxed_str(),
                markdown: fields.markdown_content.clone().into_boxed_str(),
                toc_depth: toc_depth(extension(metadata, "toc-depth")),
                toc_title: toc_title(extension(metadata, "toc-title")),
                fonts: (entry.depth == 0).then(|| Fonts::parse(extension(metadata, "fonts"))),
            })
        })
        .collect()
}

fn extension<'a>(metadata: &'a NodeMetadata, key: &str) -> Option<&'a Value> {
    metadata.extensions.get(key)
}

/// `toc-depth` is free-form metadata; accept a non-negative integer or its
/// decimal string form and ignore anything else.
fn toc_depth(value: Option<&Value>) -> Option<u32> {
    match value? {
        Value::Number(number) => number.as_u64().and_then(|depth| u32::try_from(depth).ok()),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// `toc-title` must be a non-blank string; it is trimmed and bounded.
fn toc_title(value: Option<&Value>) -> Option<Box<str>> {
    let title = value?.as_str()?.trim();
    (!title.is_empty()).then(|| title.chars().take(MAX_TOC_TITLE_CHARS).collect())
}

/// `docx-exclude` is set by boolean `true` or the string `"true"` (any case).
fn docx_excluded(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(excluded)) => *excluded,
        Some(Value::String(text)) => text.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Tracks an excluded node while a depth-first traversal passes through its
/// subtree: every following entry deeper than it belongs to it.
#[derive(Default)]
struct ExcludedSubtrees {
    excluded_depth: Option<u32>,
}

impl ExcludedSubtrees {
    fn keep(&mut self, depth: u32, excluded: bool) -> bool {
        if let Some(excluded_depth) = self.excluded_depth {
            if depth > excluded_depth {
                return false;
            }
            self.excluded_depth = None;
        }
        // The selected export root (depth zero) is always kept.
        if excluded && depth > 0 {
            self.excluded_depth = Some(depth);
            return false;
        }
        true
    }
}

/// Word supports outline heading styles `Heading1` through `Heading9`.
const MAX_HEADING_LEVEL: u32 = 9;

/// Default heading of the table-of-contents page.
const DEFAULT_TOC_TITLE: &str = "Table of contents";

/// Builds a complete `.docx` document from depth-first export nodes.
#[must_use]
pub(crate) fn build_docx(nodes: &[ExportNode]) -> Vec<u8> {
    let title = nodes.first().map_or("", |node| &*node.title);
    let fonts = nodes
        .first()
        .and_then(|root| root.fonts.clone())
        .unwrap_or_default();
    let char_widths = fonts.char_widths();
    // Depth zero is the root (Heading1), so at most eight levels below it
    // still have a Word outline heading of their own.
    let toc_depth = nodes
        .first()
        .and_then(|root| root.toc_depth)
        .filter(|depth| *depth > 0)
        .map(|depth| depth.min(MAX_HEADING_LEVEL - 1));
    let mut body = String::new();
    for (index, node) in nodes.iter().enumerate() {
        let level = node.depth.saturating_add(1).min(MAX_HEADING_LEVEL);
        let in_toc = toc_depth.is_some_and(|depth| (1..=depth).contains(&node.depth));
        heading(
            &mut body,
            level,
            &node.title,
            toc_depth.is_some() && node.depth == 1,
            in_toc.then_some(index),
        );
        render_markdown(
            &mut body,
            strip_title_heading(&node.markdown, &node.title),
            char_widths,
        );
        if let (0, Some(depth)) = (index, toc_depth) {
            let title = node.toc_title.as_deref().unwrap_or(DEFAULT_TOC_TITLE);
            table_of_contents(&mut body, nodes, depth, title);
        }
    }
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
         <w:body>{body}<w:sectPr><w:footerReference w:type=\"default\" r:id=\"rId3\"/>\
         <w:pgSz w:w=\"11906\" w:h=\"16838\"/>\
         <w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" \
         w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/></w:sectPr></w:body></w:document>"
    );
    let core = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{}</dc:title></cp:coreProperties>",
        escape(title)
    );

    let mut archive = ZipWriter::default();
    archive.add("[Content_Types].xml", CONTENT_TYPES.as_bytes());
    archive.add("_rels/.rels", ROOT_RELATIONSHIPS.as_bytes());
    archive.add("docProps/core.xml", core.as_bytes());
    archive.add(
        "word/_rels/document.xml.rels",
        DOCUMENT_RELATIONSHIPS.as_bytes(),
    );
    archive.add("word/styles.xml", styles(&fonts).as_bytes());
    archive.add("word/settings.xml", SETTINGS.as_bytes());
    archive.add("word/footer1.xml", FOOTER.as_bytes());
    archive.add("word/document.xml", document.as_bytes());
    archive.finish()
}

/// A node heading; `page_break` starts it on a new page and `bookmark`
/// makes it a table-of-contents target.
fn heading(body: &mut String, level: u32, title: &str, page_break: bool, bookmark: Option<usize>) {
    let _ = write!(body, "<w:p><w:pPr><w:pStyle w:val=\"Heading{level}\"/>");
    if page_break {
        body.push_str("<w:pageBreakBefore/>");
    }
    body.push_str("</w:pPr>");
    if let Some(id) = bookmark {
        let _ = write!(
            body,
            "<w:bookmarkStart w:id=\"{id}\" w:name=\"{}\"/>",
            toc_bookmark(id)
        );
    }
    let _ = write!(
        body,
        "<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r>",
        escape(title)
    );
    if let Some(id) = bookmark {
        let _ = write!(body, "<w:bookmarkEnd w:id=\"{id}\"/>");
    }
    body.push_str("</w:p>");
}

fn toc_bookmark(id: usize) -> String {
    format!("_Toc{id:08}")
}

/// Emits the table of contents on its own page: one paragraph per node at
/// depths `1..=depth`, linked to that node's heading bookmark, with a dotted
/// leader and a live `PAGEREF` field for its page number. Live page fields,
/// rather than a Word `TOC` field, are used because `LibreOffice` shows a `TOC`
/// field's stored text verbatim while it computes `PAGEREF` itself; Word
/// fills them in through the update-fields-on-open setting.
fn table_of_contents(body: &mut String, nodes: &[ExportNode], depth: u32, title: &str) {
    let _ = write!(
        body,
        "<w:p><w:pPr><w:pStyle w:val=\"TOCHeading\"/><w:pageBreakBefore/></w:pPr>\
         <w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
        escape(title)
    );
    for (index, node) in nodes.iter().enumerate() {
        if !(1..=depth).contains(&node.depth) {
            continue;
        }
        // Entry styles follow heading levels: depth one is Heading2/TOC2.
        let bookmark = toc_bookmark(index);
        let _ = write!(
            body,
            "<w:p><w:pPr><w:pStyle w:val=\"TOC{}\"/><w:tabs><w:tab w:val=\"right\" w:leader=\"dot\" w:pos=\"{TABLE_WIDTH_TWIPS}\"/></w:tabs></w:pPr>\
             <w:hyperlink w:anchor=\"{bookmark}\" w:history=\"1\"><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r><w:r><w:tab/></w:r>",
            node.depth + 1,
            escape(&node.title)
        );
        field_begin(body, &format!(" PAGEREF {bookmark} \\h "));
        body.push_str("<w:r><w:fldChar w:fldCharType=\"end\"/></w:r></w:hyperlink></w:p>");
    }
}

/// Opens a complex field and moves to its result part.
fn field_begin(body: &mut String, instruction: &str) {
    let _ = write!(
        body,
        "<w:r><w:fldChar w:fldCharType=\"begin\"/></w:r><w:r><w:instrText xml:space=\"preserve\">{}</w:instrText></w:r>\
         <w:r><w:fldChar w:fldCharType=\"separate\"/></w:r>",
        escape(instruction)
    );
}

/// Most `MDTree` nodes begin with `# <title>`; repeating it under the node's
/// own heading would duplicate every title in the document.
fn strip_title_heading<'a>(markdown: &'a str, title: &str) -> &'a str {
    let trimmed = markdown.trim_start();
    let (first_line, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    match first_line.trim_end().strip_prefix("# ") {
        Some(heading) if heading.trim() == title.trim() => rest,
        _ => markdown,
    }
}

#[derive(Clone, Copy, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent Markdown inline formatting flags, freely combined"
)]
struct RunStyle {
    bold: bool,
    italic: bool,
    code: bool,
    highlight: bool,
}

/// Background of `==highlighted==` text, matching the `MDTree` viewer's mark.
const HIGHLIGHT_FILL: &str = "FEF08A";

struct Run {
    text: String,
    style: RunStyle,
}

fn plain(text: &str) -> Run {
    Run {
        text: text.to_owned(),
        style: RunStyle::default(),
    }
}

fn render_markdown(body: &mut String, markdown: &str, char_widths: CharWidths) {
    let mut pending: Vec<&str> = Vec::new();
    let mut table: Vec<&str> = Vec::new();
    let mut in_code = false;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if !in_code && trimmed.starts_with('|') {
            flush_paragraph(body, &mut pending);
            table.push(trimmed);
            continue;
        }
        flush_table(body, &mut table, char_widths);
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush_paragraph(body, &mut pending);
            in_code = !in_code;
            continue;
        }
        if in_code {
            let mut run = plain(line);
            run.style.code = true;
            paragraph(body, Some("CodeBlock"), &[run]);
            continue;
        }
        if trimmed.is_empty() {
            flush_paragraph(body, &mut pending);
            continue;
        }
        if let Some(heading) = markdown_heading(trimmed) {
            flush_paragraph(body, &mut pending);
            paragraph(body, Some("ContentHeading"), &inline_runs(heading));
        } else if let Some(item) = list_item(trimmed) {
            flush_paragraph(body, &mut pending);
            let mut runs = vec![plain(item.marker)];
            runs.extend(inline_runs(item.text));
            paragraph(body, Some("ListItem"), &runs);
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            flush_paragraph(body, &mut pending);
            paragraph(body, Some("Quote"), &inline_runs(quote.trim_start()));
        } else if is_horizontal_rule(trimmed) {
            flush_paragraph(body, &mut pending);
        } else {
            pending.push(trimmed);
        }
    }
    flush_paragraph(body, &mut pending);
    flush_table(body, &mut table, char_widths);
}

/// Word table width for an A4 page with 2.54 cm margins, in twentieths of a point.
const TABLE_WIDTH_TWIPS: usize = 9026;

/// Borders and cell margins are written on every table as well as in the
/// `KnowledgeTable` style, because some viewers ignore custom table styles.
const TABLE_BORDERS: &str = "<w:tblBorders><w:top w:val=\"nil\"/><w:left w:val=\"nil\"/><w:bottom w:val=\"nil\"/><w:right w:val=\"nil\"/>\
<w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"E5E7EB\"/><w:insideV w:val=\"nil\"/></w:tblBorders>\
<w:tblCellMar><w:top w:w=\"110\" w:type=\"dxa\"/><w:left w:w=\"0\" w:type=\"dxa\"/><w:bottom w:w=\"110\" w:type=\"dxa\"/><w:right w:w=\"170\" w:type=\"dxa\"/></w:tblCellMar>";

/// Renders consecutive `|`-delimited lines as a Word table. A second-line
/// delimiter row (`|---|:--:|`) marks the first row as a bold, repeating
/// header and sets column alignment; without one every row is body text.
fn flush_table(body: &mut String, lines: &mut Vec<&str>, char_widths: CharWidths) {
    if lines.is_empty() {
        return;
    }
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(lines.len());
    let mut alignments: Vec<Option<&'static str>> = Vec::new();
    let mut has_header = false;
    for (index, line) in lines.iter().enumerate() {
        if index == 1 && is_table_separator(line) {
            has_header = true;
            alignments = table_cells(line)
                .iter()
                .map(|cell| column_alignment(cell))
                .collect();
            continue;
        }
        if is_table_separator(line) {
            continue;
        }
        rows.push(table_cells(line));
    }
    lines.clear();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return;
    }
    let column_widths = column_widths(&rows, columns, char_widths);

    let _ = write!(
        body,
        "<w:tbl><w:tblPr><w:tblStyle w:val=\"KnowledgeTable\"/><w:tblW w:w=\"5000\" w:type=\"pct\"/>{TABLE_BORDERS}\
         <w:tblLook w:val=\"0020\" w:firstRow=\"1\" w:lastRow=\"0\" w:firstColumn=\"0\" w:lastColumn=\"0\" w:noHBand=\"1\" w:noVBand=\"1\"/></w:tblPr><w:tblGrid>",
    );
    for column_width in &column_widths {
        let _ = write!(body, "<w:gridCol w:w=\"{column_width}\"/>");
    }
    body.push_str("</w:tblGrid>");
    for (row_index, row) in rows.iter().enumerate() {
        let header = has_header && row_index == 0;
        body.push_str(if header {
            "<w:tr><w:trPr><w:cantSplit/><w:tblHeader/></w:trPr>"
        } else {
            "<w:tr><w:trPr><w:cantSplit/></w:trPr>"
        });
        for (column, column_width) in column_widths.iter().enumerate() {
            let _ = write!(
                body,
                "<w:tc><w:tcPr><w:tcW w:w=\"{column_width}\" w:type=\"dxa\"/>{}</w:tcPr>",
                if header {
                    "<w:tcBorders><w:bottom w:val=\"single\" w:sz=\"8\" w:space=\"0\" w:color=\"CBD5E1\"/></w:tcBorders>"
                } else {
                    ""
                }
            );
            let mut runs = inline_runs(row.get(column).map_or("", String::as_str));
            if header {
                for run in &mut runs {
                    run.style.bold = true;
                }
            }
            let alignment = alignments.get(column).copied().flatten();
            paragraph_with(body, Some("TableText"), alignment, &runs);
            body.push_str("</w:tc>");
        }
        body.push_str("</w:tr>");
    }
    // Word merges directly adjacent tables; a spacer keeps them separate.
    body.push_str("</w:tbl><w:p><w:pPr><w:pStyle w:val=\"TableSpacer\"/></w:pPr></w:p>");
}

/// Twips per [`display_width`] unit, and the cell's right padding from the
/// table style.
const TWIPS_PER_WIDTH_UNIT: usize = 10;
const CELL_PADDING_TWIPS: usize = 170;

/// Sizes columns like a simple autofit: each column first gets room for its
/// longest unbreakable word, then the remaining page width is shared in
/// proportion to how much more each column's longest cell needs. When even
/// the words do not fit, the minimums are scaled down together.
fn column_widths(rows: &[Vec<String>], columns: usize, char_widths: CharWidths) -> Vec<usize> {
    let to_twips = |width: usize| width * TWIPS_PER_WIDTH_UNIT + CELL_PADDING_TWIPS;
    let cells = |column: usize| rows.iter().filter_map(move |row| row.get(column));
    let minimums: Vec<usize> = (0..columns)
        .map(|column| {
            to_twips(
                cells(column)
                    .flat_map(|cell| cell.split_whitespace())
                    .map(|word| display_width(word, char_widths))
                    .max()
                    .unwrap_or(0)
                    .max(36),
            )
        })
        .collect();
    let desired: Vec<usize> = (0..columns)
        .map(|column| {
            to_twips(
                cells(column)
                    .map(|cell| display_width(cell, char_widths))
                    .max()
                    .unwrap_or(0),
            )
            .max(minimums[column])
        })
        .collect();
    let minimum_total: usize = minimums.iter().sum();
    if minimum_total >= TABLE_WIDTH_TWIPS {
        return capped_widths(&minimums);
    }
    let spare = TABLE_WIDTH_TWIPS - minimum_total;
    let extra: Vec<usize> = desired
        .iter()
        .zip(&minimums)
        .map(|(desired, minimum)| desired - minimum)
        .collect();
    let extra_total: usize = extra.iter().sum();
    minimums
        .iter()
        .zip(&extra)
        .map(|(minimum, extra)| {
            minimum
                + (spare * extra)
                    .checked_div(extra_total)
                    .unwrap_or(spare / columns)
        })
        .collect()
}

/// Keeps narrow columns intact and shrinks only the widest ones to a common
/// cap, so a long code value wraps instead of crushing short columns.
fn capped_widths(minimums: &[usize]) -> Vec<usize> {
    let mut sorted = minimums.to_vec();
    sorted.sort_unstable();
    let mut remaining = TABLE_WIDTH_TWIPS;
    let mut cap = 0;
    for (index, width) in sorted.iter().enumerate() {
        let columns_left = sorted.len() - index;
        if width * columns_left > remaining {
            cap = remaining / columns_left;
            break;
        }
        remaining -= width;
    }
    minimums.iter().map(|width| (*width).min(cap)).collect()
}

/// Estimated rendered width in units of ten twips, from per-character
/// estimates for proportional text and monospace code (see `CharWidths`).
/// Markdown markers are not rendered.
fn display_width(cell: &str, char_widths: CharWidths) -> usize {
    let mut width = 0;
    let mut in_code = false;
    for character in cell.chars() {
        match character {
            '`' => in_code = !in_code,
            '*' | '_' if !in_code => {}
            _ => {
                width += if in_code {
                    char_widths.code
                } else {
                    char_widths.text
                };
            }
        }
    }
    width
}

/// Splits one table row on unescaped `|` outside inline code spans.
fn table_cells(line: &str) -> Vec<String> {
    let inner = line.trim();
    let inner = inner.strip_prefix('|').unwrap_or(inner);
    let inner = inner
        .strip_suffix('|')
        .filter(|_| !inner.ends_with("\\|"))
        .unwrap_or(inner);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    let mut characters = inner.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&'|') => {
                current.push('|');
                characters.next();
            }
            '`' => {
                in_code = !in_code;
                current.push(character);
            }
            '|' if !in_code => cells.push(std::mem::take(&mut current).trim().to_owned()),
            _ => current.push(character),
        }
    }
    cells.push(current.trim().to_owned());
    cells
}

fn column_alignment(delimiter: &str) -> Option<&'static str> {
    match (delimiter.starts_with(':'), delimiter.ends_with(':')) {
        (true, true) => Some("center"),
        (false, true) => Some("right"),
        _ => None,
    }
}

fn flush_paragraph(body: &mut String, pending: &mut Vec<&str>) {
    if pending.is_empty() {
        return;
    }
    let joined = pending.join(" ");
    pending.clear();
    paragraph(body, None, &inline_runs(&joined));
}

fn markdown_heading(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    line[hashes..]
        .strip_prefix(' ')
        .map(|text| text.trim_end_matches('#').trim())
}

struct ListItem<'a> {
    marker: &'a str,
    text: &'a str,
}

fn list_item(line: &str) -> Option<ListItem<'_>> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(text) = line.strip_prefix(bullet) {
            return Some(ListItem {
                marker: "• ", text
            });
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        if rest.starts_with(". ") || rest.starts_with(") ") {
            return Some(ListItem {
                marker: &line[..digits + 2],
                text: &rest[2..],
            });
        }
    }
    None
}

fn is_horizontal_rule(line: &str) -> bool {
    line.len() >= 3
        && ["-", "*", "_"].iter().any(|mark| {
            line.chars()
                .filter(|character| !character.is_whitespace())
                .all(|character| character.to_string() == *mark)
        })
}

fn is_table_separator(line: &str) -> bool {
    line.contains('-')
        && line
            .chars()
            .all(|character| matches!(character, '|' | '-' | ':' | ' '))
}

/// Splits Markdown inline emphasis (`**bold**`, `*italic*`, `_italic_`,
/// `==highlight==`, `` `code` ``) into styled runs and reduces
/// `[text](url)` links to `text`.
fn inline_runs(text: &str) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut current = String::new();
    let mut style = RunStyle::default();
    let mut rest = text;
    while let Some(character) = rest.chars().next() {
        let push_current = |runs: &mut Vec<Run>, current: &mut String, style: RunStyle| {
            if !current.is_empty() {
                runs.push(Run {
                    text: std::mem::take(current),
                    style,
                });
            }
        };
        if style.code {
            if character == '`' {
                push_current(&mut runs, &mut current, style);
                style.code = false;
            } else {
                current.push(character);
            }
            rest = &rest[character.len_utf8()..];
            continue;
        }
        if rest.starts_with("**") || rest.starts_with("__") {
            push_current(&mut runs, &mut current, style);
            style.bold = !style.bold;
            rest = &rest[2..];
            continue;
        }
        if character == '`' {
            push_current(&mut runs, &mut current, style);
            style.code = true;
            rest = &rest[1..];
            continue;
        }
        if rest.starts_with("==")
            && highlight_delimiter(&text[..text.len() - rest.len()], rest, style.highlight)
        {
            push_current(&mut runs, &mut current, style);
            style.highlight = !style.highlight;
            rest = &rest[2..];
            continue;
        }
        if character == '*' || (character == '_' && word_boundary_emphasis(&current, rest)) {
            push_current(&mut runs, &mut current, style);
            style.italic = !style.italic;
            rest = &rest[1..];
            continue;
        }
        if character == '[' {
            if let Some((label, after)) = markdown_link(rest) {
                current.push_str(label);
                rest = after;
                continue;
            }
        }
        current.push(character);
        rest = &rest[character.len_utf8()..];
    }
    if !current.is_empty() {
        runs.push(Run {
            text: current,
            style,
        });
    }
    runs
}

/// Like other emphasis, `==` opens only before non-space text that has a
/// closing `==` later, and closes only after non-space text, so comparisons
/// such as `a == b` stay literal.
fn highlight_delimiter(preceding: &str, rest: &str, open: bool) -> bool {
    let after = &rest[2..];
    if open {
        preceding
            .chars()
            .last()
            .is_some_and(|character| !character.is_whitespace())
    } else {
        after
            .chars()
            .next()
            .is_some_and(|character| !character.is_whitespace())
            && after.find("==").is_some_and(|close| {
                after[..close]
                    .chars()
                    .last()
                    .is_some_and(|character| !character.is_whitespace())
            })
    }
}

/// `snake_case_words` must not toggle italics; only `_` at a word edge does.
fn word_boundary_emphasis(before: &str, rest: &str) -> bool {
    let previous_is_word = before.chars().last().is_some_and(char::is_alphanumeric);
    let next_is_word = rest[1..].chars().next().is_some_and(char::is_alphanumeric);
    !(previous_is_word && next_is_word)
}

fn markdown_link(text: &str) -> Option<(&str, &str)> {
    let close = text.find("](")?;
    let label = &text[1..close];
    if label.contains('[') {
        return None;
    }
    let after_open = &text[close + 2..];
    let end = after_open.find(')')?;
    Some((label, &after_open[end + 1..]))
}

fn paragraph(body: &mut String, style: Option<&str>, runs: &[Run]) {
    paragraph_with(body, style, None, runs);
}

fn paragraph_with(body: &mut String, style: Option<&str>, alignment: Option<&str>, runs: &[Run]) {
    body.push_str("<w:p>");
    if style.is_some() || alignment.is_some() {
        body.push_str("<w:pPr>");
        if let Some(style) = style {
            let _ = write!(body, "<w:pStyle w:val=\"{style}\"/>");
        }
        if let Some(alignment) = alignment {
            let _ = write!(body, "<w:jc w:val=\"{alignment}\"/>");
        }
        body.push_str("</w:pPr>");
    }
    for run in runs {
        body.push_str("<w:r>");
        let style = run.style;
        if style.bold || style.italic || style.code || style.highlight {
            // CT_RPr is an ordered sequence: style, bold, italic, shading.
            // Inline code takes its font from the `CodeChar` style.
            body.push_str("<w:rPr>");
            if style.code {
                body.push_str("<w:rStyle w:val=\"CodeChar\"/>");
            }
            if style.bold {
                body.push_str("<w:b/>");
            }
            if style.italic {
                body.push_str("<w:i/>");
            }
            if style.highlight {
                let _ = write!(
                    body,
                    "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{HIGHLIGHT_FILL}\"/>"
                );
            }
            body.push_str("</w:rPr>");
        }
        body.push_str("<w:t xml:space=\"preserve\">");
        body.push_str(&escape(&run.text));
        body.push_str("</w:t></w:r>");
    }
    body.push_str("</w:p>");
}

/// XML-escapes text and drops characters XML 1.0 cannot represent.
pub(crate) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\t' => escaped.push(' '),
            character if character < ' ' || matches!(character, '\u{FFFE}' | '\u{FFFF}') => {}
            character => escaped.push(character),
        }
    }
    escaped
}

const CONTENT_TYPES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/word/settings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml\"/>\
<Override PartName=\"/word/footer1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
</Types>";

const ROOT_RELATIONSHIPS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
</Relationships>";

const DOCUMENT_RELATIONSHIPS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings\" Target=\"settings.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer\" Target=\"footer1.xml\"/>\
</Relationships>";

/// Every page's footer: the current page number, right-aligned, as a live
/// `PAGE` field.
const FOOTER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:ftr xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:p><w:pPr><w:pStyle w:val=\"Footer\"/><w:jc w:val=\"right\"/></w:pPr>\
<w:r><w:fldChar w:fldCharType=\"begin\"/></w:r><w:r><w:instrText xml:space=\"preserve\"> PAGE </w:instrText></w:r>\
<w:r><w:fldChar w:fldCharType=\"separate\"/></w:r><w:r><w:t>1</w:t></w:r><w:r><w:fldChar w:fldCharType=\"end\"/></w:r>\
</w:p></w:ftr>";

/// Asks Word to refresh fields on open, which fills the table of contents'
/// page numbers (`LibreOffice` computes them regardless).
const SETTINGS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:updateFields w:val=\"true\"/><w:defaultTabStop w:val=\"708\"/><w:characterSpacingControl w:val=\"doNotCompress\"/>\
</w:settings>";

/// Word styles for every paragraph and run kind the export writes, with run
/// properties taken from the export root's `fonts` (or the defaults).
fn styles(fonts: &Fonts) -> String {
    let mut styles = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
         <w:docDefaults><w:rPrDefault>{body}</w:rPrDefault>\
         <w:pPrDefault><w:pPr><w:spacing w:after=\"120\" w:line=\"264\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault></w:docDefaults>\
         <w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"ContentHeading\"><w:name w:val=\"Content Heading\"/><w:basedOn w:val=\"Normal\"/>\
         <w:pPr><w:keepNext/><w:spacing w:before=\"160\" w:after=\"80\"/></w:pPr><w:rPr><w:b/><w:bCs/></w:rPr></w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"ListItem\"><w:name w:val=\"List Item\"/><w:basedOn w:val=\"Normal\"/>\
         <w:pPr><w:spacing w:after=\"40\"/><w:ind w:left=\"567\" w:hanging=\"283\"/></w:pPr></w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"Quote\"><w:name w:val=\"Quote\"/><w:basedOn w:val=\"Normal\"/>\
         <w:pPr><w:ind w:left=\"567\"/></w:pPr><w:rPr><w:i/><w:color w:val=\"555555\"/></w:rPr></w:style>\
         <w:style w:type=\"table\" w:styleId=\"KnowledgeTable\"><w:name w:val=\"Knowledge Table\"/>\
         <w:tblPr><w:tblBorders><w:top w:val=\"nil\"/><w:left w:val=\"nil\"/><w:bottom w:val=\"nil\"/><w:right w:val=\"nil\"/>\
         <w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"E5E7EB\"/><w:insideV w:val=\"nil\"/></w:tblBorders>\
         <w:tblCellMar><w:top w:w=\"110\" w:type=\"dxa\"/><w:left w:w=\"0\" w:type=\"dxa\"/><w:bottom w:w=\"110\" w:type=\"dxa\"/><w:right w:w=\"170\" w:type=\"dxa\"/></w:tblCellMar></w:tblPr></w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"Footer\"><w:name w:val=\"footer\"/><w:basedOn w:val=\"Normal\"/>\
         <w:pPr><w:spacing w:after=\"0\"/></w:pPr>{footer}</w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"TableText\"><w:name w:val=\"Table Text\"/><w:basedOn w:val=\"Normal\"/>\
         <w:pPr><w:spacing w:after=\"0\"/></w:pPr>{table}</w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"TableSpacer\"><w:name w:val=\"Table Spacer\"/><w:basedOn w:val=\"Normal\"/>\
         <w:pPr><w:spacing w:after=\"120\" w:line=\"120\" w:lineRule=\"exact\"/></w:pPr><w:rPr><w:sz w:val=\"4\"/></w:rPr></w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"CodeBlock\"><w:name w:val=\"Code Block\"/><w:basedOn w:val=\"Normal\"/>\
         <w:pPr><w:spacing w:after=\"0\" w:line=\"240\" w:lineRule=\"auto\"/><w:ind w:left=\"284\"/></w:pPr>\
         {code_block}</w:style>\
         <w:style w:type=\"character\" w:styleId=\"CodeChar\"><w:name w:val=\"Code Char\"/>{inline_code}</w:style>",
        body = fonts.body().style_rpr(true, "<w:lang w:val=\"en-US\"/>"),
        footer = fonts.footer().style_rpr(false, ""),
        table = fonts.table().style_rpr(false, ""),
        code_block = fonts.code_block().style_rpr(false, ""),
        inline_code = fonts.inline_code().style_rpr(false, ""),
    );
    // The table of contents title is styled like the root's own heading.
    let _ = write!(
        styles,
        "<w:style w:type=\"paragraph\" w:styleId=\"TOCHeading\"><w:name w:val=\"TOC Heading\"/>\
         <w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:pPr><w:keepNext/><w:spacing w:before=\"360\" w:after=\"240\"/></w:pPr>\
         {}</w:style>",
        fonts.heading(1).style_rpr(false, "")
    );
    for level in 1..=MAX_HEADING_LEVEL {
        // Entries for depth one (Heading2/TOC2) sit flush left.
        let indent = level.saturating_sub(2) * 360;
        let _ = write!(
            styles,
            "<w:style w:type=\"paragraph\" w:styleId=\"TOC{level}\"><w:name w:val=\"toc {level}\"/>\
             <w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:uiPriority w:val=\"39\"/>\
             <w:pPr><w:spacing w:after=\"60\"/><w:ind w:left=\"{indent}\"/></w:pPr>{}</w:style>",
            fonts.toc(level as usize).style_rpr(false, "")
        );
    }
    for level in 1..=MAX_HEADING_LEVEL {
        let _ = write!(
            styles,
            "<w:style w:type=\"paragraph\" w:styleId=\"Heading{level}\"><w:name w:val=\"heading {level}\"/>\
             <w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/>\
             <w:pPr><w:keepNext/><w:spacing w:before=\"{before}\" w:after=\"120\"/><w:outlineLvl w:val=\"{outline}\"/></w:pPr>\
             {rpr}</w:style>",
            before = if level == 1 { 360 } else { 240 },
            outline = level - 1,
            rpr = fonts.heading(level as usize).style_rpr(false, ""),
        );
    }
    styles.push_str("</w:styles>");
    styles
}

const ZIP_VERSION: u16 = 10;
const ZIP_UTF8_NAMES: u16 = 1 << 11;
/// Entries are stored uncompressed, keeping the writer dependency-free.
const ZIP_STORED: u16 = 0;
const DOS_TIME: u16 = 0;
/// 1980-01-01, the earliest representable DOS date.
const DOS_DATE: u16 = (1 << 5) | 1;

/// Just enough of the ZIP format for an OOXML package: stored entries, a
/// central directory, and a fixed 1980-01-01 timestamp so identical trees
/// produce byte-identical exports.
#[derive(Default)]
struct ZipWriter {
    output: Vec<u8>,
    central_directory: Vec<u8>,
    entries: u16,
}

/// CRC-32 (IEEE 802.3, reflected polynomial `0xEDB88320`) as ZIP requires.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

impl ZipWriter {
    fn add(&mut self, name: &str, data: &[u8]) {
        let compressed = data;
        let crc = crc32(data);
        let offset = u32::try_from(self.output.len()).expect("export archive exceeds 4 GiB");
        let compressed_size = u32::try_from(compressed.len()).expect("entry exceeds 4 GiB");
        let size = u32::try_from(data.len()).expect("entry exceeds 4 GiB");
        let name_length = u16::try_from(name.len()).expect("entry name exceeds 64 KiB");

        let local = &mut self.output;
        local.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        for value in [ZIP_VERSION, ZIP_UTF8_NAMES, ZIP_STORED, DOS_TIME, DOS_DATE] {
            local.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, compressed_size, size] {
            local.extend_from_slice(&value.to_le_bytes());
        }
        local.extend_from_slice(&name_length.to_le_bytes());
        local.extend_from_slice(&0_u16.to_le_bytes());
        local.extend_from_slice(name.as_bytes());
        local.extend_from_slice(compressed);

        let central = &mut self.central_directory;
        central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        for value in [
            ZIP_VERSION,
            ZIP_VERSION,
            ZIP_UTF8_NAMES,
            ZIP_STORED,
            DOS_TIME,
            DOS_DATE,
        ] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, compressed_size, size] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        // name length, extra, comment, disk start, internal attributes
        for value in [name_length, 0, 0, 0, 0] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        central.extend_from_slice(&0_u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
        self.entries += 1;
    }

    fn finish(mut self) -> Vec<u8> {
        let directory_offset =
            u32::try_from(self.output.len()).expect("export archive exceeds 4 GiB");
        let directory_size =
            u32::try_from(self.central_directory.len()).expect("central directory exceeds 4 GiB");
        self.output.extend_from_slice(&self.central_directory);
        self.output
            .extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
        for value in [0, 0, self.entries, self.entries] {
            self.output.extend_from_slice(&u16::to_le_bytes(value));
        }
        self.output.extend_from_slice(&directory_size.to_le_bytes());
        self.output
            .extend_from_slice(&directory_offset.to_le_bytes());
        self.output.extend_from_slice(&0_u16.to_le_bytes());
        self.output
    }
}

#[cfg(test)]
mod tests {
    use super::ExportNode;

    use super::{
        build_docx, column_widths, escape, inline_runs, render_markdown, strip_title_heading,
        table_cells, TABLE_WIDTH_TWIPS,
    };
    use crate::docx_fonts::{CharWidths, Fonts};

    fn node(depth: u32, title: &str, markdown: &str) -> ExportNode {
        ExportNode {
            depth,
            title: title.into(),
            markdown: markdown.into(),
            toc_depth: None,
            toc_title: None,
            fonts: None,
        }
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn metadata_values_are_parsed_leniently_but_strictly_typed() {
        use serde_json::json;
        assert_eq!(super::toc_depth(Some(&json!(2))), Some(2));
        assert_eq!(super::toc_depth(Some(&json!(" 3 "))), Some(3));
        assert_eq!(super::toc_depth(Some(&json!(-1))), None);
        assert_eq!(
            super::toc_title(Some(&json!("  Contents "))).as_deref(),
            Some("Contents")
        );
        assert_eq!(super::toc_title(Some(&json!("  "))), None);
        assert!(super::docx_excluded(Some(&json!(true))));
        assert!(super::docx_excluded(Some(&json!("TRUE"))));
        assert!(!super::docx_excluded(Some(&json!("yes"))));
        assert!(!super::docx_excluded(None));
    }

    #[test]
    fn an_excluded_node_drops_its_whole_subtree_but_never_the_root() {
        let walk = [
            (0, true),
            (1, false),
            (2, true),
            (3, false),
            (3, true),
            (2, false),
            (1, true),
            (2, false),
            (1, false),
        ];
        let mut exclusions = super::ExcludedSubtrees::default();
        let kept: Vec<usize> = walk
            .iter()
            .enumerate()
            .filter(|(_, (depth, excluded))| exclusions.keep(*depth, *excluded))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(kept, vec![0, 1, 5, 8]);
    }

    #[test]
    fn a_document_is_a_zip_package_with_every_required_part() {
        let bytes = build_docx(&[node(0, "Root", "# Root\n\nIntro"), node(1, "Child", "Body")]);
        assert_eq!(&bytes[..4], b"PK\x03\x04");
        for part in [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/document.xml",
            "word/styles.xml",
            "word/footer1.xml",
        ] {
            assert!(
                bytes
                    .windows(part.len())
                    .any(|window| window == part.as_bytes()),
                "missing {part}"
            );
        }
    }

    #[test]
    fn identical_trees_produce_identical_documents() {
        let nodes = [node(0, "Root", "Same content")];
        assert_eq!(build_docx(&nodes), build_docx(&nodes));
    }

    #[test]
    fn a_leading_heading_repeating_the_title_is_removed() {
        assert_eq!(strip_title_heading("# Title\n\nBody", "Title"), "\nBody");
        assert_eq!(
            strip_title_heading("# Other\nBody", "Title"),
            "# Other\nBody"
        );
    }

    #[test]
    fn inline_markdown_becomes_styled_runs_and_links_keep_their_label() {
        let runs = inline_runs("a **b** `c_d` [e](https://x) snake_case");
        let texts: Vec<_> = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(texts, vec!["a ", "b", " ", "c_d", " e snake_case"]);
        assert!(runs[1].style.bold);
        assert!(runs[3].style.code);
    }

    #[test]
    fn table_rows_split_on_unescaped_pipes_outside_code_spans() {
        assert_eq!(
            table_cells("| `a|b` | c \\| d | **e** |"),
            vec!["`a|b`", "c | d", "**e**"]
        );
    }

    #[test]
    fn a_markdown_table_becomes_a_word_table_with_a_bold_repeating_header() {
        let mut body = String::new();
        render_markdown(
            &mut body,
            "Intro\n| Lauks | Vērtība |\n|---|--:|\n| Adrese | Rīga |\n| E-pasts | *precizējams* |\nAfter",
            CharWidths::DEFAULT,
        );
        assert_eq!(body.matches("<w:tbl>").count(), 1);
        assert_eq!(body.matches("<w:tr>").count(), 3);
        assert_eq!(body.matches("<w:tblHeader/>").count(), 1);
        assert_eq!(body.matches("<w:gridCol ").count(), 2);
        assert!(body.contains("<w:b/></w:rPr><w:t xml:space=\"preserve\">Lauks"));
        assert!(body.contains("<w:jc w:val=\"right\"/>"));
        assert!(body.contains("<w:i/></w:rPr><w:t xml:space=\"preserve\">precizējams"));
        assert!(!body.contains("---"));
        assert!(body.ends_with("After</w:t></w:r></w:p>"));
    }

    #[test]
    fn columns_fit_their_longest_word_and_share_the_rest_by_content() {
        let rows = vec![
            vec!["Nr.".to_owned(), "Apraksts".to_owned()],
            vec!["1".to_owned(), "garš ".repeat(40)],
        ];
        let widths = column_widths(&rows, 2, CharWidths::DEFAULT);
        assert_eq!(widths.iter().sum::<usize>(), TABLE_WIDTH_TWIPS);
        assert!(widths[0] < widths[1] / 5);
    }

    #[test]
    fn overlong_code_columns_shrink_without_crushing_short_columns() {
        let rows = vec![vec![
            "`".to_owned() + &"x".repeat(80) + "`",
            "1/1".to_owned(),
            "`".to_owned() + &"y".repeat(80) + "`",
        ]];
        let widths = column_widths(&rows, 3, CharWidths::DEFAULT);
        assert!(widths.iter().sum::<usize>() <= TABLE_WIDTH_TWIPS);
        assert_eq!(widths[1], 36 * 10 + 170);
        assert_eq!(widths[0], widths[2]);
    }

    fn toc_root(depth: u32) -> ExportNode {
        ExportNode {
            toc_depth: Some(depth),
            ..node(0, "Root", "# Root\n\nCover page")
        }
    }

    fn document_xml(nodes: &[ExportNode]) -> String {
        package_part(nodes, "word/document.xml")
    }

    fn package_part(nodes: &[ExportNode], part: &str) -> String {
        let bytes = build_docx(nodes);
        // Entries are stored, so each part follows its local header.
        let name = part.as_bytes();
        // A name can also appear inside [Content_Types].xml, so match only a
        // local file header (signature `PK\x03\x04`, name 30 bytes later).
        let header = (0..bytes.len() - 30 - name.len())
            .find(|&at| {
                bytes[at..at + 4] == *b"PK\x03\x04" && bytes[at + 30..at + 30 + name.len()] == *name
            })
            .expect("package part");
        let size = u32::from_le_bytes(bytes[header + 18..header + 22].try_into().unwrap()) as usize;
        let start = header + 30 + name.len();
        String::from_utf8(bytes[start..start + size].to_vec()).expect("utf-8 part")
    }

    #[test]
    fn a_toc_root_gets_cover_toc_and_first_level_pages_in_order() {
        let xml = document_xml(&[
            toc_root(2),
            node(1, "One", "a"),
            node(2, "One.One", "b"),
            node(3, "Too deep", "c"),
            node(1, "Two", "d"),
        ]);
        let cover = xml.find("Cover page").expect("cover");
        let toc = xml.find("Table of contents").expect("toc title");
        let first = xml
            .find("_Toc00000001\"/><w:r><w:t xml:space=\"preserve\">One<")
            .expect("heading");
        assert!(cover < toc && toc < first);
        // TOC heading plus the two first-level headings start new pages.
        assert_eq!(xml.matches("<w:pageBreakBefore/>").count(), 3);
        // Depths one and two are listed and bookmarked; depth three is not.
        assert_eq!(xml.matches("PAGEREF ").count(), 3);
        assert_eq!(xml.matches("<w:bookmarkStart ").count(), 3);
        assert!(!xml.contains("PAGEREF _Toc00000003"));
        assert!(xml.contains("<w:pStyle w:val=\"TOC3\"/>"));
    }

    #[test]
    fn toc_title_metadata_replaces_the_default_heading() {
        let xml = document_xml(&[
            ExportNode {
                toc_title: Some("Saturs & <pielikumi>".into()),
                ..toc_root(1)
            },
            node(1, "One", "a"),
        ]);
        assert!(xml.contains(">Saturs &amp; &lt;pielikumi&gt;<"));
        assert!(!xml.contains("Table of contents"));
    }

    #[test]
    fn root_fonts_reach_the_styles_and_child_fonts_are_ignored() {
        let fonts = serde_json::json!({
            "body": {"family": "Arial"},
            "headings": {"family": "Georgia"},
            "code": {"family": "Courier New", "size": 9}
        });
        let styles = package_part(
            &[
                ExportNode {
                    fonts: Some(Fonts::parse(Some(&fonts))),
                    ..node(0, "Root", "Uses `code`")
                },
                ExportNode {
                    fonts: Some(Fonts::parse(Some(
                        &serde_json::json!({"body": {"family": "Comic Sans MS"}}),
                    ))),
                    ..node(1, "Child", "x")
                },
            ],
            "word/styles.xml",
        );
        assert!(styles.contains("<w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Arial\""));
        assert!(styles.contains("w:styleId=\"Heading2\"><w:name w:val=\"heading 2\"/>"));
        assert_eq!(
            styles.matches("w:ascii=\"Georgia\"").count(),
            10,
            "nine headings and the TOC title"
        );
        assert!(styles.contains("w:styleId=\"CodeChar\"><w:name w:val=\"Code Char\"/><w:rPr><w:rFonts w:ascii=\"Courier New\""));
        assert!(!styles.contains("Comic Sans MS"));
        let document = document_xml(&[node(0, "Root", "Uses `code`")]);
        assert!(document.contains(
            "<w:rPr><w:rStyle w:val=\"CodeChar\"/></w:rPr><w:t xml:space=\"preserve\">code<"
        ));
    }

    #[test]
    fn without_toc_depth_there_is_no_toc_and_no_forced_pages() {
        for nodes in [
            vec![node(0, "Root", "x"), node(1, "One", "a")],
            vec![toc_root(0), node(1, "One", "a")],
        ] {
            let xml = document_xml(&nodes);
            assert!(!xml.contains("Table of contents"));
            assert!(!xml.contains("<w:pageBreakBefore/>"));
            assert!(!xml.contains("PAGEREF"));
        }
    }

    #[test]
    fn double_equals_marks_highlighted_text_but_comparisons_stay_literal() {
        let runs = inline_runs("==Nav informācijas== un a == b, ==**svarīgi**==");
        let highlighted: Vec<_> = runs
            .iter()
            .filter(|run| run.style.highlight)
            .map(|run| (run.text.as_str(), run.style.bold))
            .collect();
        assert_eq!(
            highlighted,
            vec![("Nav informācijas", false), ("svarīgi", true)]
        );
        let text: String = runs.iter().map(|run| run.text.as_str()).collect();
        assert_eq!(text, "Nav informācijas un a == b, svarīgi");
    }

    #[test]
    fn untrusted_text_is_escaped_and_invalid_xml_characters_dropped() {
        assert_eq!(escape("<w:p>&\"\u{1}"), "&lt;w:p&gt;&amp;&quot;");
    }
}
