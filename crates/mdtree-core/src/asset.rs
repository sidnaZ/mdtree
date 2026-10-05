//! Workspace image assets: names, `asset:` references, and image inspection.
//!
//! Images are stored inside the workspace and referenced from Markdown with
//! standard image syntax and an `asset:` URL, optionally sized as a
//! percentage of the text width:
//!
//! ```markdown
//! ![NIS architecture](asset:NIS-arhitektura.png?width=80% "Figure 1. NIS architecture")
//! ```

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use crate::NodeHash;

/// URL scheme that refers to a workspace asset by name.
pub const ASSET_URL_SCHEME: &str = "asset:";
/// Largest accepted asset, in bytes.
pub const MAX_ASSET_BYTES: usize = 20 * 1024 * 1024;
/// Longest accepted asset name, in characters.
pub const MAX_ASSET_NAME_CHARS: usize = 128;

const ASSET_DOMAIN: &[u8] = b"mdtree-asset-v1\0";

/// A workspace-unique asset name such as `NIS-arhitektura.png`.
///
/// Names use ASCII letters, digits, `.`, `_`, and `-` only, so they can appear
/// unescaped in a Markdown link destination and a URL path, and never start
/// with `.`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AssetName(String);

impl AssetName {
    /// The validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derives a valid name from an arbitrary file name: diacritics are
    /// folded like slugs (`ā` → `a`), other invalid characters become `-`,
    /// the length is bounded, and the extension matches `media_type`.
    #[must_use]
    pub fn from_file_name(file_name: &str, media_type: MediaType) -> Self {
        let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
        let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);
        let mut cleaned = String::new();
        for character in stem
            .nfkd()
            .filter(|character| !is_combining_mark(*character))
        {
            let character = if is_name_char(character) {
                character
            } else {
                '-'
            };
            if !(character == '-' && cleaned.ends_with('-')) {
                cleaned.push(character);
            }
        }
        let cleaned = cleaned.trim_matches(['-', '.']);
        let extension = media_type.extension();
        let budget = MAX_ASSET_NAME_CHARS - extension.len() - 1;
        let stem: String = if cleaned.is_empty() {
            "image".into()
        } else {
            cleaned.chars().take(budget).collect()
        };
        Self(format!("{stem}.{extension}"))
    }

    /// `name` with `-N` inserted before the extension, for picking a free name.
    #[must_use]
    pub fn numbered(&self, number: u32) -> Self {
        let (stem, extension) = self.0.rsplit_once('.').unwrap_or((&self.0, ""));
        let suffix = format!("-{number}");
        let budget = MAX_ASSET_NAME_CHARS - suffix.len() - extension.len() - 1;
        let stem: String = stem.chars().take(budget).collect();
        if extension.is_empty() {
            Self(format!("{stem}{suffix}"))
        } else {
            Self(format!("{stem}{suffix}.{extension}"))
        }
    }
}

fn is_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

impl FromStr for AssetName {
    type Err = AssetError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > MAX_ASSET_NAME_CHARS
            || value.starts_with('.')
            || !value.chars().all(is_name_char)
        {
            return Err(AssetError::InvalidName(value.chars().take(64).collect()));
        }
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Display for AssetName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for AssetName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AssetName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Supported image formats: those both browsers and Word render natively.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum MediaType {
    /// `image/png`
    #[serde(rename = "image/png")]
    Png,
    /// `image/jpeg`
    #[serde(rename = "image/jpeg")]
    Jpeg,
    /// `image/gif`
    #[serde(rename = "image/gif")]
    Gif,
}

impl MediaType {
    /// The IANA media type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
        }
    }

    /// The conventional file extension.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Gif => "gif",
        }
    }
}

impl FromStr for MediaType {
    type Err = AssetError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "image/png" => Ok(Self::Png),
            "image/jpeg" => Ok(Self::Jpeg),
            "image/gif" => Ok(Self::Gif),
            other => Err(AssetError::UnsupportedType(other.into())),
        }
    }
}

/// Format and pixel dimensions read from an image's own header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageInfo {
    /// Detected format.
    pub media_type: MediaType,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Asset validation failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AssetError {
    /// The name contains characters other than ASCII letters, digits, `.`, `_`, `-`.
    #[error("invalid asset name {0:?}: use ASCII letters, digits, '.', '_' and '-' only")]
    InvalidName(String),
    /// The bytes are not a PNG, JPEG, or GIF image.
    #[error("unsupported image type {0}: PNG, JPEG and GIF are supported")]
    UnsupportedType(String),
    /// The image header is truncated or has zero dimensions.
    #[error("corrupt or truncated {0} image")]
    Corrupt(&'static str),
    /// The image exceeds [`MAX_ASSET_BYTES`].
    #[error("image is {0} bytes; the limit is {MAX_ASSET_BYTES}")]
    TooLarge(usize),
}

/// Identifies a supported image by its content (never its file name) and
/// reads its pixel dimensions.
///
/// # Errors
///
/// Returns [`AssetError`] for unsupported, corrupt, or oversized input.
pub fn inspect_image(bytes: &[u8]) -> Result<ImageInfo, AssetError> {
    if bytes.len() > MAX_ASSET_BYTES {
        return Err(AssetError::TooLarge(bytes.len()));
    }
    let (media_type, dimensions) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // The IHDR chunk is always first: width and height follow its header.
        let dimensions = (bytes.len() >= 24 && &bytes[12..16] == b"IHDR")
            .then(|| (be32(&bytes[16..20]), be32(&bytes[20..24])));
        (MediaType::Png, dimensions)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        let dimensions = (bytes.len() >= 10).then(|| {
            (
                u32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
                u32::from(u16::from_le_bytes([bytes[8], bytes[9]])),
            )
        });
        (MediaType::Gif, dimensions)
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        (MediaType::Jpeg, jpeg_dimensions(bytes))
    } else {
        return Err(AssetError::UnsupportedType(sniffed_label(bytes)));
    };
    match dimensions {
        Some((width, height)) if width > 0 && height > 0 => Ok(ImageInfo {
            media_type,
            width,
            height,
        }),
        _ => Err(AssetError::Corrupt(media_type.as_str())),
    }
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Walks JPEG marker segments to the first start-of-frame header.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            return None;
        }
        let marker = bytes[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        // Standalone markers carry no length.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            at += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        // SOF0–SOF15, except DHT (C4), JPG (C8) and DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if at + 9 > bytes.len() {
                return None;
            }
            let height = u16::from_be_bytes([bytes[at + 5], bytes[at + 6]]);
            let width = u16::from_be_bytes([bytes[at + 7], bytes[at + 8]]);
            return Some((u32::from(width), u32::from(height)));
        }
        at += 2 + length;
    }
    None
}

fn sniffed_label(bytes: &[u8]) -> String {
    if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        "SVG".into()
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "WebP".into()
    } else {
        "(unrecognized)".into()
    }
}

/// Content hash identifying identical image bytes.
#[must_use]
pub fn hash_asset(bytes: &[u8]) -> NodeHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ASSET_DOMAIN);
    hasher.update(bytes);
    NodeHash::new(*hasher.finalize().as_bytes())
}

/// An `asset:` image destination: the asset name and optional display width.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetUrl {
    /// Referenced asset.
    pub name: AssetName,
    /// `?width=N%`: percent of the available text width, 1–100.
    pub width_percent: Option<u8>,
}

impl AssetUrl {
    /// Parses `asset:<name>[?width=<1-100>%]`; `None` for any other URL or an
    /// invalid name. Unknown or invalid query parameters are ignored.
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let rest = url.trim().strip_prefix(ASSET_URL_SCHEME)?;
        let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
        let name = name.parse().ok()?;
        let width_percent = query
            .split('&')
            .filter_map(|pair| pair.strip_prefix("width="))
            .find_map(|value| value.trim_end_matches('%').parse::<u8>().ok())
            .filter(|percent| (1..=100).contains(percent));
        Some(Self {
            name,
            width_percent,
        })
    }
}

/// Stored asset metadata, without the image bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AssetRecord {
    /// Workspace-unique name.
    pub name: AssetName,
    /// Content hash of the image bytes, serialized as lowercase hex.
    #[serde(serialize_with = "serialize_hex")]
    pub hash: NodeHash,
    /// Detected image format.
    pub media_type: MediaType,
    /// Image size in bytes.
    pub byte_size: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Creation time in Unix epoch milliseconds.
    pub created_at: u64,
    /// Last replacement time in Unix epoch milliseconds.
    pub updated_at: u64,
}

fn serialize_hex<S: Serializer>(hash: &NodeHash, serializer: S) -> Result<S::Ok, S::Error> {
    use std::fmt::Write as _;
    let hex = hash.as_bytes().iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    serializer.serialize_str(&hex)
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard padded base64, as used for asset bytes in JSON snapshots.
#[must_use]
pub fn encode_base64(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (triple >> (18 - 6 * index)) & 0x3F;
                encoded.push(char::from(BASE64_ALPHABET[sextet as usize]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

/// Decodes standard padded base64; `None` for malformed input.
#[must_use]
pub fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let encoded = encoded.as_bytes();
    if !encoded.len().is_multiple_of(4) {
        return None;
    }
    let value = |byte: u8| -> Option<u32> {
        BASE64_ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)
            .and_then(|position| u32::try_from(position).ok())
    };
    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
    let chunks = encoded.len() / 4;
    for (index, chunk) in encoded.chunks(4).enumerate() {
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || (padding > 0 && index + 1 != chunks) {
            return None;
        }
        let mut triple = 0_u32;
        for byte in &chunk[..4 - padding] {
            triple = (triple << 6) | value(*byte)?;
        }
        triple <<= 6 * padding;
        let bytes = triple.to_be_bytes();
        decoded.extend_from_slice(&bytes[1..4 - padding]);
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::{
        decode_base64, encode_base64, hash_asset, inspect_image, AssetError, AssetName, AssetUrl,
        MediaType,
    };

    /// A 3×2 PNG header (signature + IHDR) is all `inspect_image` reads.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    #[test]
    fn images_are_identified_by_content_with_their_dimensions() {
        let info = inspect_image(&png(3, 2)).expect("png");
        assert_eq!(
            (info.media_type, info.width, info.height),
            (MediaType::Png, 3, 2)
        );
        let gif = b"GIF89a\x05\x00\x04\x00\x00";
        assert_eq!(inspect_image(gif).expect("gif").width, 5);
        // SOI, an APP0 segment, then SOF0 with height 0x0102 and width 0x0304.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01,
            0x02, 0x03, 0x04,
        ];
        let info = inspect_image(&jpeg).expect("jpeg");
        assert_eq!((info.width, info.height), (0x0304, 0x0102));
        assert_eq!(
            inspect_image(b"<svg xmlns=\"\"/>"),
            Err(AssetError::UnsupportedType("SVG".into()))
        );
        assert_eq!(
            inspect_image(&png(0, 2)),
            Err(AssetError::Corrupt("image/png"))
        );
    }

    #[test]
    fn names_are_restricted_and_derivable_from_any_file_name() {
        assert!("NIS-arhitektura.png".parse::<AssetName>().is_ok());
        for invalid in ["", ".hidden.png", "a b.png", "a/b.png", "ā.png"] {
            assert!(invalid.parse::<AssetName>().is_err(), "{invalid}");
        }
        assert_eq!(
            AssetName::from_file_name("C:\\tmp\\Ekrāna  attēls (2).PNG", MediaType::Png).as_str(),
            "Ekrana-attels-2.png"
        );
        assert_eq!(
            AssetName::from_file_name("..jpg", MediaType::Jpeg).as_str(),
            "image.jpeg"
        );
        let name: AssetName = "diagram.png".parse().expect("name");
        assert_eq!(name.numbered(2).as_str(), "diagram-2.png");
    }

    #[test]
    fn asset_urls_carry_a_name_and_an_optional_bounded_width() {
        let url = AssetUrl::parse("asset:diagram.png?width=80%").expect("url");
        assert_eq!(
            (url.name.as_str(), url.width_percent),
            ("diagram.png", Some(80))
        );
        assert_eq!(
            AssetUrl::parse("asset:d.png?width=150%")
                .expect("url")
                .width_percent,
            None
        );
        assert_eq!(
            AssetUrl::parse("asset:d.png").expect("url").width_percent,
            None
        );
        assert!(AssetUrl::parse("https://example.com/d.png").is_none());
        assert!(AssetUrl::parse("asset:bad name.png").is_none());
    }

    #[test]
    fn base64_round_trips_and_rejects_malformed_input() {
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foob", &[0, 255, 128, 7, 9]] {
            assert_eq!(
                decode_base64(&encode_base64(sample)).as_deref(),
                Some(sample)
            );
        }
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert!(decode_base64("Zm9").is_none());
        assert!(decode_base64("Zm==Zm9v").is_none());
        assert!(decode_base64("Zm9*").is_none());
    }

    #[test]
    fn identical_bytes_share_a_hash() {
        assert_eq!(hash_asset(b"x"), hash_asset(b"x"));
        assert_ne!(hash_asset(b"x"), hash_asset(b"y"));
    }
}
