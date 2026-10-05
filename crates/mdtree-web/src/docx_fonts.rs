//! The export root's `fonts` metadata: per-role font overrides for the
//! generated Word document.
//!
//! ```json
//! "fonts": {
//!   "body":      { "family": "Arial", "size": 11, "color": "222222" },
//!   "headings":  { "family": "Georgia", "color": "1F3864", "bold": true },
//!   "heading-2": { "size": 16, "italic": true },
//!   "code":      { "family": "Courier New", "size": 9.5 },
//!   "table":     { "size": 10 },
//!   "toc":       { "size": 11 },
//!   "footer":    { "size": 8, "color": "888888" },
//!   "caption":   { "size": 9, "italic": true }
//! }
//! ```
//!
//! Every role and property is optional. A role states only what it changes
//! and inherits the rest: `heading-N` from `headings`, and `headings`,
//! `table`, `toc`, `footer` and `caption` from `body` (through Word's style
//! inheritance). `code` keeps a monospace family unless one is given. Invalid
//! values are ignored so a typo falls back to the default instead of
//! breaking the export.

use std::fmt::Write as _;

use serde_json::Value;

use crate::docx_export::escape;

/// Word supports outline heading styles `Heading1` through `Heading9`.
pub(crate) const HEADING_LEVELS: usize = 9;
const MAX_FAMILY_CHARS: usize = 64;
const MIN_SIZE_POINTS: f64 = 4.0;
const MAX_SIZE_POINTS: f64 = 96.0;

const DEFAULT_BODY_FAMILY: &str = "Calibri";
const DEFAULT_CODE_FAMILY: &str = "Consolas";
/// Sizes are in half points, Word's unit for `w:sz`.
const DEFAULT_BODY_SIZE: u32 = 22;
const DEFAULT_CODE_BLOCK_SIZE: u32 = 18;
const DEFAULT_INLINE_CODE_SIZE: u32 = 20;
const DEFAULT_FOOTER_SIZE: u32 = 18;
const DEFAULT_HEADING_COLOR: &str = "1F3864";
const DEFAULT_FOOTER_COLOR: &str = "6B7280";
const DEFAULT_CAPTION_SIZE: u32 = 20;
const DEFAULT_CAPTION_COLOR: &str = "555555";

/// One role's font properties; `None` means "inherit".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FontSpec {
    pub(crate) family: Option<Box<str>>,
    /// Size in half points.
    pub(crate) size: Option<u32>,
    /// Upper-case `RRGGBB`.
    pub(crate) color: Option<Box<str>>,
    pub(crate) bold: Option<bool>,
    pub(crate) italic: Option<bool>,
}

impl FontSpec {
    fn parse(value: Option<&Value>) -> Self {
        let Some(Value::Object(properties)) = value else {
            return Self::default();
        };
        Self {
            family: properties.get("family").and_then(family),
            size: properties.get("size").and_then(size),
            color: properties.get("color").and_then(color),
            bold: properties.get("bold").and_then(flag),
            italic: properties.get("italic").and_then(flag),
        }
    }

    /// `self` with every unset property taken from `fallback`.
    #[must_use]
    pub(crate) fn or(&self, fallback: &Self) -> Self {
        Self {
            family: self.family.clone().or_else(|| fallback.family.clone()),
            size: self.size.or(fallback.size),
            color: self.color.clone().or_else(|| fallback.color.clone()),
            bold: self.bold.or(fallback.bold),
            italic: self.italic.or(fallback.italic),
        }
    }

    /// The `w:rPr` children for a style, in `CT_RPr` schema order, followed
    /// by `trailing` (elements that sort after `w:szCs`, such as `w:lang`).
    /// An explicit `false` is written so it can switch off an inherited
    /// bold or italic.
    pub(crate) fn style_rpr(&self, east_asia: bool, trailing: &str) -> String {
        let mut rpr = String::new();
        if let Some(family) = &self.family {
            let family = escape(family);
            let _ = write!(
                rpr,
                "<w:rFonts w:ascii=\"{family}\" w:hAnsi=\"{family}\" w:cs=\"{family}\""
            );
            if east_asia {
                let _ = write!(rpr, " w:eastAsia=\"{family}\"");
            }
            rpr.push_str("/>");
        }
        match self.bold {
            Some(true) => rpr.push_str("<w:b/><w:bCs/>"),
            Some(false) => rpr.push_str("<w:b w:val=\"0\"/><w:bCs w:val=\"0\"/>"),
            None => {}
        }
        match self.italic {
            Some(true) => rpr.push_str("<w:i/><w:iCs/>"),
            Some(false) => rpr.push_str("<w:i w:val=\"0\"/><w:iCs w:val=\"0\"/>"),
            None => {}
        }
        if let Some(color) = &self.color {
            let _ = write!(rpr, "<w:color w:val=\"{color}\"/>");
        }
        if let Some(size) = self.size {
            let _ = write!(rpr, "<w:sz w:val=\"{size}\"/><w:szCs w:val=\"{size}\"/>");
        }
        rpr.push_str(trailing);
        if rpr.is_empty() {
            rpr
        } else {
            format!("<w:rPr>{rpr}</w:rPr>")
        }
    }
}

/// Every configurable role, as parsed from the export root's `fonts`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Fonts {
    body: FontSpec,
    headings: FontSpec,
    heading: [FontSpec; HEADING_LEVELS],
    code: FontSpec,
    table: FontSpec,
    toc: FontSpec,
    footer: FontSpec,
    caption: FontSpec,
}

impl Fonts {
    /// Parses the `fonts` metadata object; anything else yields defaults.
    pub(crate) fn parse(value: Option<&Value>) -> Self {
        let Some(Value::Object(roles)) = value else {
            return Self::default();
        };
        let role = |name: &str| FontSpec::parse(roles.get(name));
        Self {
            body: role("body"),
            headings: role("headings"),
            heading: std::array::from_fn(|index| role(&format!("heading-{}", index + 1))),
            code: role("code"),
            table: role("table"),
            toc: role("toc"),
            footer: role("footer"),
            caption: role("caption"),
        }
    }

    /// Document defaults (Word's `docDefaults`), inherited by every style.
    pub(crate) fn body(&self) -> FontSpec {
        self.body.or(&FontSpec {
            family: Some(DEFAULT_BODY_FAMILY.into()),
            size: Some(DEFAULT_BODY_SIZE),
            ..FontSpec::default()
        })
    }

    /// Node heading `level` (1 = export root); the table of contents title
    /// uses level 1.
    pub(crate) fn heading(&self, level: usize) -> FontSpec {
        let default_size = 36_u32
            .saturating_sub(
                u32::try_from(level.saturating_sub(1))
                    .unwrap_or(u32::MAX)
                    .saturating_mul(3),
            )
            .max(22);
        self.heading[level.clamp(1, HEADING_LEVELS) - 1]
            .or(&self.headings)
            .or(&FontSpec {
                size: Some(default_size),
                color: Some(DEFAULT_HEADING_COLOR.into()),
                bold: Some(true),
                ..FontSpec::default()
            })
    }

    pub(crate) fn code_block(&self) -> FontSpec {
        self.code.or(&FontSpec {
            family: Some(DEFAULT_CODE_FAMILY.into()),
            size: Some(DEFAULT_CODE_BLOCK_SIZE),
            ..FontSpec::default()
        })
    }

    pub(crate) fn inline_code(&self) -> FontSpec {
        self.code.or(&FontSpec {
            family: Some(DEFAULT_CODE_FAMILY.into()),
            size: Some(DEFAULT_INLINE_CODE_SIZE),
            ..FontSpec::default()
        })
    }

    /// Table cells; unset properties inherit from `body` through Word.
    pub(crate) fn table(&self) -> FontSpec {
        self.table.clone()
    }

    /// Table-of-contents entry `level` (2 = the root's children, bold).
    pub(crate) fn toc(&self, level: usize) -> FontSpec {
        self.toc.or(&FontSpec {
            bold: (level == 2).then_some(true),
            ..FontSpec::default()
        })
    }

    /// Image captions (an image's Markdown title).
    pub(crate) fn caption(&self) -> FontSpec {
        self.caption.or(&FontSpec {
            size: Some(DEFAULT_CAPTION_SIZE),
            color: Some(DEFAULT_CAPTION_COLOR.into()),
            italic: Some(true),
            ..FontSpec::default()
        })
    }

    pub(crate) fn footer(&self) -> FontSpec {
        self.footer.or(&FontSpec {
            size: Some(DEFAULT_FOOTER_SIZE),
            color: Some(DEFAULT_FOOTER_COLOR.into()),
            ..FontSpec::default()
        })
    }

    /// Character-width estimates for table column sizing, scaled from the
    /// 11 pt text and 10 pt code the defaults were calibrated for.
    pub(crate) fn char_widths(&self) -> CharWidths {
        let text = self
            .table
            .size
            .or(self.body.size)
            .unwrap_or(DEFAULT_BODY_SIZE);
        let code = self.inline_code().size.unwrap_or(DEFAULT_INLINE_CODE_SIZE);
        CharWidths {
            text: scaled(CharWidths::DEFAULT.text, text, DEFAULT_BODY_SIZE),
            code: scaled(CharWidths::DEFAULT.code, code, DEFAULT_INLINE_CODE_SIZE),
        }
    }
}

/// Estimated rendered character widths, in units of ten twips.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CharWidths {
    pub(crate) text: usize,
    pub(crate) code: usize,
}

impl CharWidths {
    /// About 120 twips per 11 pt proportional character (allowing for bold
    /// and substitute fonts) and 130 per 10 pt monospace character
    /// (substitute monospace fonts are wider than Consolas).
    pub(crate) const DEFAULT: Self = Self { text: 12, code: 13 };
}

fn scaled(width: usize, size: u32, calibrated_size: u32) -> usize {
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    let calibrated_size = usize::try_from(calibrated_size).unwrap_or(1);
    (width.saturating_mul(size))
        .div_ceil(calibrated_size)
        .max(1)
}

/// A non-blank font family name without control characters.
fn family(value: &Value) -> Option<Box<str>> {
    let family = value.as_str()?.trim();
    (!family.is_empty()
        && family.chars().count() <= MAX_FAMILY_CHARS
        && !family.chars().any(char::is_control))
    .then(|| family.into())
}

/// Points as a number or numeric string, 4–96, rounded to half points.
fn size(value: &Value) -> Option<u32> {
    let points = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    if !(MIN_SIZE_POINTS..=MAX_SIZE_POINTS).contains(&points) {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded to 8..=192 half points above"
    )]
    Some((points * 2.0).round() as u32)
}

/// `RRGGBB`, with or without a leading `#`, any case.
fn color(value: &Value) -> Option<Box<str>> {
    let hex = value.as_str()?.trim();
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    (hex.len() == 6 && hex.chars().all(|character| character.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_uppercase().into())
}

/// A boolean, or its `"true"`/`"false"` string form in any case.
fn flag(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CharWidths, FontSpec, Fonts};

    #[test]
    fn invalid_values_are_ignored_and_valid_ones_normalized() {
        let fonts = Fonts::parse(Some(&json!({
            "body": {"family": "  Arial ", "size": "10.5", "color": "#1a2b3c", "bold": "TRUE"},
            "code": {"family": "", "size": 200, "color": "blue", "italic": 1},
            "unknown-role": {"family": "Ignored"}
        })));
        assert_eq!(
            fonts.body,
            FontSpec {
                family: Some("Arial".into()),
                size: Some(21),
                color: Some("1A2B3C".into()),
                bold: Some(true),
                italic: None,
            }
        );
        assert_eq!(fonts.code, FontSpec::default());
        assert_eq!(Fonts::parse(Some(&json!("Arial"))), Fonts::default());
        assert_eq!(Fonts::parse(None), Fonts::default());
    }

    #[test]
    fn heading_levels_inherit_from_headings_then_defaults() {
        let fonts = Fonts::parse(Some(&json!({
            "headings": {"family": "Georgia", "color": "000000"},
            "heading-2": {"size": 16, "bold": false}
        })));
        let level_two = fonts.heading(2);
        assert_eq!(level_two.family.as_deref(), Some("Georgia"));
        assert_eq!(level_two.color.as_deref(), Some("000000"));
        assert_eq!(level_two.size, Some(32));
        assert_eq!(level_two.bold, Some(false));
        let level_one = fonts.heading(1);
        assert_eq!(level_one.size, Some(36));
        assert_eq!(level_one.bold, Some(true));
    }

    #[test]
    fn style_properties_follow_schema_order_and_can_switch_off_inheritance() {
        let spec = FontSpec {
            family: Some("A & B".into()),
            size: Some(20),
            color: Some("112233".into()),
            bold: Some(false),
            italic: Some(true),
        };
        assert_eq!(
            spec.style_rpr(false, ""),
            "<w:rPr><w:rFonts w:ascii=\"A &amp; B\" w:hAnsi=\"A &amp; B\" w:cs=\"A &amp; B\"/>\
             <w:b w:val=\"0\"/><w:bCs w:val=\"0\"/><w:i/><w:iCs/><w:color w:val=\"112233\"/>\
             <w:sz w:val=\"20\"/><w:szCs w:val=\"20\"/></w:rPr>"
        );
        assert_eq!(FontSpec::default().style_rpr(false, ""), "");
    }

    #[test]
    fn table_width_estimates_scale_with_font_size() {
        assert_eq!(Fonts::default().char_widths(), CharWidths::DEFAULT);
        let larger = Fonts::parse(Some(&json!({"body": {"size": 22}, "code": {"size": 20}})));
        assert_eq!(larger.char_widths(), CharWidths { text: 24, code: 26 });
        let table_only = Fonts::parse(Some(&json!({"body": {"size": 22}, "table": {"size": 11}})));
        assert_eq!(table_only.char_widths().text, 12);
    }
}
