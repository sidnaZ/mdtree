//! Embedding workspace image assets in exported Word documents.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use mdtree_core::{AssetName, MediaType};
use mdtree_sqlite::{SqliteStore, StoreError};

use crate::docx_export::{escape, ExportNode};

/// English Metric Units per pixel at 96 DPI, Word's default image density.
const EMU_PER_PIXEL: u64 = 9525;
/// EMU per twip (1/1440 inch = 635 EMU).
pub(crate) const EMU_PER_TWIP: u64 = 635;

/// One image file inside the package, shared by every asset name whose
/// bytes are identical.
#[derive(Clone, Debug)]
pub(crate) struct ImagePart {
    /// Relationship ID referenced by `a:blip r:embed`.
    pub(crate) rel_id: String,
    /// Path inside the package, relative to `word/`.
    pub(crate) path: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) media_type: MediaType,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// The images an export references, keyed by asset name.
#[derive(Clone, Debug, Default)]
pub(crate) struct ImageSet {
    by_name: BTreeMap<AssetName, usize>,
    parts: Vec<ImagePart>,
}

impl ImageSet {
    /// Loads every asset referenced by `nodes`; names that do not exist are
    /// simply absent (the document shows a placeholder for them).
    pub(crate) fn load(store: &SqliteStore, nodes: &[ExportNode]) -> Result<Self, StoreError> {
        let mut set = Self::default();
        let mut part_by_hash = BTreeMap::new();
        for node in nodes {
            for reference in mdtree_markdown::extract_asset_references(&node.markdown) {
                if set.by_name.contains_key(&reference.name) {
                    continue;
                }
                let Some((record, bytes)) = store.asset_bytes(&reference.name)? else {
                    continue;
                };
                let index = *part_by_hash
                    .entry(*record.hash.as_bytes())
                    .or_insert_with(|| {
                        let hex = record.hash.as_bytes()[..8].iter().fold(
                            String::new(),
                            |mut hex, byte| {
                                let _ = write!(hex, "{byte:02x}");
                                hex
                            },
                        );
                        set.parts.push(ImagePart {
                            rel_id: format!("rIdImage{}", set.parts.len() + 1),
                            path: format!("media/{hex}.{}", record.media_type.extension()),
                            bytes,
                            media_type: record.media_type,
                            width: record.width,
                            height: record.height,
                        });
                        set.parts.len() - 1
                    });
                set.by_name.insert(reference.name, index);
            }
        }
        Ok(set)
    }

    /// A set holding one image under `name`, for renderer tests.
    #[cfg(test)]
    pub(crate) fn single(name: &str, width: u32, height: u32) -> Self {
        let mut set = Self::default();
        set.parts.push(ImagePart {
            rel_id: "rIdImage1".into(),
            path: "media/0011223344556677.png".into(),
            bytes: b"\x89PNG".to_vec(),
            media_type: MediaType::Png,
            width,
            height,
        });
        set.by_name.insert(name.parse().expect("asset name"), 0);
        set
    }

    pub(crate) fn get(&self, name: &AssetName) -> Option<&ImagePart> {
        self.by_name.get(name).map(|index| &self.parts[*index])
    }

    pub(crate) fn parts(&self) -> &[ImagePart] {
        &self.parts
    }

    /// `<Relationship>` elements for `word/_rels/document.xml.rels`.
    pub(crate) fn relationships(&self) -> String {
        self.parts.iter().fold(String::new(), |mut xml, part| {
            let _ = write!(
                xml,
                "<Relationship Id=\"{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"{}\"/>",
                part.rel_id, part.path
            );
            xml
        })
    }

    /// `<Default>` content types for the image formats in use.
    pub(crate) fn content_types(&self) -> String {
        let mut seen = Vec::new();
        let mut xml = String::new();
        for part in &self.parts {
            if !seen.contains(&part.media_type) {
                seen.push(part.media_type);
                let _ = write!(
                    xml,
                    "<Default Extension=\"{}\" ContentType=\"{}\"/>",
                    part.media_type.extension(),
                    part.media_type.as_str()
                );
            }
        }
        xml
    }
}

/// Displayed size in EMU: natural size at 96 DPI, or `width_percent` of
/// `max_width`, never wider than `max_width`, aspect ratio kept.
pub(crate) fn extent(part: &ImagePart, width_percent: Option<u8>, max_width: u64) -> (u64, u64) {
    let natural = u64::from(part.width) * EMU_PER_PIXEL;
    let width = width_percent
        .map_or(natural, |percent| max_width * u64::from(percent) / 100)
        .min(max_width)
        .max(1);
    let height = (width * u64::from(part.height) / u64::from(part.width.max(1))).max(1);
    (width, height)
}

/// One inline `DrawingML` picture run.
pub(crate) fn drawing(part: &ImagePart, alt: &str, (cx, cy): (u64, u64), id: u32) -> String {
    let alt = escape(alt);
    format!(
        "<w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">\
         <wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:docPr id=\"{id}\" name=\"Picture {id}\" descr=\"{alt}\"/>\
         <wp:cNvGraphicFramePr><a:graphicFrameLocks xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" noChangeAspect=\"1\"/></wp:cNvGraphicFramePr>\
         <a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
         <a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
         <pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
         <pic:nvPicPr><pic:cNvPr id=\"{id}\" name=\"Picture {id}\" descr=\"{alt}\"/><pic:cNvPicPr/></pic:nvPicPr>\
         <pic:blipFill><a:blip r:embed=\"{rel}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
         <pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
         <a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>\
         </a:graphicData></a:graphic></wp:inline></w:drawing></w:r>",
        rel = part.rel_id,
    )
}

#[cfg(test)]
mod tests {
    use mdtree_core::MediaType;

    use super::{extent, ImagePart, EMU_PER_PIXEL};

    fn part(width: u32, height: u32) -> ImagePart {
        ImagePart {
            rel_id: "rIdImage1".into(),
            path: "media/x.png".into(),
            bytes: Vec::new(),
            media_type: MediaType::Png,
            width,
            height,
        }
    }

    #[test]
    fn images_keep_their_aspect_ratio_and_never_exceed_the_available_width() {
        let max = 1000 * EMU_PER_PIXEL;
        assert_eq!(
            extent(&part(200, 100), None, max),
            (200 * EMU_PER_PIXEL, 100 * EMU_PER_PIXEL)
        );
        assert_eq!(extent(&part(4000, 1000), None, max), (max, max / 4));
        assert_eq!(extent(&part(200, 100), Some(50), max), (max / 2, max / 4));
    }
}
