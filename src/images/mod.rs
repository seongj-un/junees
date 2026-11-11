//! Images the model looks at: files `read_file` reads, `/image` and
//! `--image` attach, and pictures attached to Discord messages.
//!
//! An image goes to the API as an `image` content block with a base64
//! source (vision docs, `curl
//! https://platform.claude.com/docs/en/build-with-claude/vision.md`,
//! "Base64-encoded image example"):
//! `{"type": "image", "source": {"type": "base64", "media_type":
//! "image/png", "data": "..."}}`. Inside a tool result the same block goes
//! into the result's `content` list (handle-tool-calls docs: "These content
//! blocks can use the `text`, `image`, `document`, or `search_result`
//! types").
//!
//! Only the formats the API reads are accepted, recognized by their first
//! bytes rather than by the file name. Nothing is resized (that would need
//! an image library); an image over the limits below is refused with a
//! message saying so.
//!
//! Session journals keep each image once, as a file of the session's own
//! folder, and refer to it ([`store`]).

pub mod discord;
pub mod store;

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The largest image file Junees sends, in bytes. The API takes images up
/// to 10 MB base64-encoded (5 MB on Amazon Bedrock and Google-managed
/// Vertex AI; vision docs, "Request limits"); 5 MiB of file is about 7 MB
/// of base64, and keeps every request of a long session well under the
/// 128 MB request limit.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// "The maximum dimensions for each image are 8000x8000 px" (vision docs,
/// "Request limits").
pub const MAX_DIMENSION: u32 = 8000;

/// The most visual tokens one image costs: the high-resolution tier's
/// limit (Claude 4.7 and later; vision docs, "Resolution and token cost").
/// Other models cap it lower, so estimates err high.
const MAX_IMAGE_TOKENS: u64 = 4784;

/// The high-resolution tier's long-edge limit: larger images are scaled
/// down to it before they are counted.
const MAX_LONG_EDGE: f64 = 2576.0;

/// Base64 characters decoded to find an image's size for estimates: the
/// size of a JPEG comes after its metadata segments, which can hold a
/// thumbnail.
const SIZE_PREFIX_CHARS: usize = 256 * 1024;

/// The text of an input that is nothing but images (`/image` without a
/// question, a Discord message of attachments only).
pub const NO_TEXT: &str = "(sent with no text)";

/// Why the claude-code backend takes no images.
pub const NOT_ON_CLAUDE_CODE: &str = "images are sent on the api backend only: the claude-code \
     backend passes Claude Code text (ask it to read the file instead, or /backend api)";

/// The file `/image` and `--image` name: `~` and `~/...` in `home`, a
/// relative path in `cwd`.
pub fn resolve_path(path: &str, cwd: &Path, home: Option<&Path>) -> std::path::PathBuf {
    if let Some(home) = home {
        if path == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    cwd.join(path)
}

/// The image formats the API reads ("Supported formats": JPEG, PNG, GIF
/// and WebP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl MediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaType::Png => "image/png",
            MediaType::Jpeg => "image/jpeg",
            MediaType::Gif => "image/gif",
            MediaType::Webp => "image/webp",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "image/png" => MediaType::Png,
            "image/jpeg" => MediaType::Jpeg,
            "image/gif" => MediaType::Gif,
            "image/webp" => MediaType::Webp,
            _ => return None,
        })
    }

    /// The file name extension of the format.
    pub fn extension(self) -> &'static str {
        match self {
            MediaType::Png => "png",
            MediaType::Jpeg => "jpg",
            MediaType::Gif => "gif",
            MediaType::Webp => "webp",
        }
    }

    /// The format of `bytes`, by their magic bytes.
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(MediaType::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(MediaType::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(MediaType::Gif)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(MediaType::Webp)
        } else {
            None
        }
    }
}

/// Width and height in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// Where an image block's data is.
#[derive(Clone, PartialEq, Eq)]
pub enum Source {
    /// The image itself, base64-encoded, as the API takes it.
    Base64(Arc<str>),
    /// A file in the image folder of the session journal the block was read
    /// from, named by the SHA-256 of its bytes ([`store`]). Only journals
    /// hold these; [`crate::session::Session::load`] reads the files back.
    Stored { sha256: String },
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Megabytes of base64 help nobody reading a test failure.
            Source::Base64(data) => write!(f, "Base64({} chars)", data.len()),
            Source::Stored { sha256 } => write!(f, "Stored({sha256})"),
        }
    }
}

/// An image content block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub media_type: MediaType,
    pub source: Source,
}

impl Image {
    /// The image in `bytes`, if the API can take it: a supported format,
    /// at most [`MAX_IMAGE_BYTES`] and [`MAX_DIMENSION`] on each side. The
    /// error says what is wrong in a sentence.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, Size), String> {
        let (media_type, size) = check(bytes)?;
        let image = Self {
            media_type,
            source: Source::Base64(STANDARD.encode(bytes).into()),
        };
        Ok((image, size))
    }

    /// The image's bytes, if it is held inline and its base64 decodes.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        match &self.source {
            Source::Base64(data) => STANDARD.decode(data.as_bytes()).ok(),
            Source::Stored { .. } => None,
        }
    }

    /// Width and height, read from the start of the image.
    pub fn size(&self) -> Option<Size> {
        let Source::Base64(data) = &self.source else {
            return None;
        };
        // Whole groups of four characters decode on their own.
        let end = data.len().min(SIZE_PREFIX_CHARS) / 4 * 4;
        let prefix = STANDARD.decode(&data.as_bytes()[..end]).ok()?;
        size(self.media_type, &prefix)
    }

    /// A rough count of the tokens the image costs: `ceil(width / 28) *
    /// ceil(height / 28)` visual tokens after scaling it down to the
    /// high-resolution tier's limits (vision docs, "Resolution and token
    /// cost"), or that tier's most when the size is unknown.
    pub fn estimate_tokens(&self) -> u64 {
        let Some(Size { width, height }) = self.size() else {
            return MAX_IMAGE_TOKENS;
        };
        let (width, height) = (f64::from(width), f64::from(height));
        let scale = (MAX_LONG_EDGE / width.max(height)).min(1.0);
        let patches = |side: f64| ((side * scale) / 28.0).ceil().max(1.0) as u64;
        (patches(width) * patches(height)).min(MAX_IMAGE_TOKENS)
    }

    /// How the image is named in text: `image/png, 640x480 px`.
    pub fn describe(&self) -> String {
        match self.size() {
            Some(size) => format!(
                "{}, {}x{} px",
                self.media_type.as_str(),
                size.width,
                size.height
            ),
            None => self.media_type.as_str().to_string(),
        }
    }
}

/// The format and size of the image in `bytes`, or why the API would not
/// take it.
pub fn check(bytes: &[u8]) -> Result<(MediaType, Size), String> {
    let media_type = MediaType::detect(bytes)
        .ok_or("not a PNG, JPEG, GIF or WebP image (the formats the model reads)")?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    let size = size(media_type, bytes).ok_or_else(|| {
        format!(
            "cannot read the size of this {} image; the file may be damaged",
            media_type.as_str()
        )
    })?;
    if size.width == 0 || size.height == 0 {
        return Err("the image has no pixels".to_string());
    }
    if size.width > MAX_DIMENSION || size.height > MAX_DIMENSION {
        return Err(format!(
            "the image is {}x{} px; the API takes images up to {MAX_DIMENSION}x{MAX_DIMENSION} px. \
             Scale it down first (for example with ImageMagick: convert in.png -resize 2000x2000 out.png)",
            size.width, size.height
        ));
    }
    Ok((media_type, size))
}

/// The message for an image of `len` bytes, over [`MAX_IMAGE_BYTES`].
pub fn too_large(len: u64) -> String {
    format!(
        "the image is {}; images are sent up to {} MB. Make it smaller first (for example \
         with ImageMagick: convert in.png -resize 2000x2000 -quality 85 out.jpg)",
        human_bytes(len),
        MAX_IMAGE_BYTES / (1024 * 1024)
    )
}

/// `512 B`, `12.3 KB`, `4.5 MB`.
pub fn human_bytes(len: u64) -> String {
    match len {
        0..1024 => format!("{len} B"),
        1024..1_048_576 => format!("{:.1} KB", len as f64 / 1024.0),
        _ => format!("{:.1} MB", len as f64 / 1_048_576.0),
    }
}

/// Reads the image file at `path` (`/image`, `--image`). The error names
/// the file and what is wrong.
pub fn load_file(path: &Path) -> Result<(Image, String), String> {
    let shown = path.display();
    let file = std::fs::File::open(path).map_err(|err| format!("cannot read {shown}: {err}"))?;
    let meta = file
        .metadata()
        .map_err(|err| format!("cannot read {shown}: {err}"))?;
    if !meta.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!("{shown}: {}", too_large(meta.len())));
    }
    let mut bytes = Vec::new();
    // A file that grew since: read one byte past the limit to notice.
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("cannot read {shown}: {err}"))?;
    let (image, size) = Image::from_bytes(&bytes).map_err(|err| format!("{shown}: {err}"))?;
    let line = format!(
        "{shown} ({}, {}x{} px, {})",
        image.media_type.as_str(),
        size.width,
        size.height,
        human_bytes(bytes.len() as u64)
    );
    Ok((image, line))
}

/// Width and height from the header of an image of `media_type`, if
/// `bytes` reach far enough.
pub fn size(media_type: MediaType, bytes: &[u8]) -> Option<Size> {
    let be16 = |at: usize| Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let le16 = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let le24 = |at: usize| {
        let b = bytes.get(at..at + 3)?;
        Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
    };
    let size = |width: u32, height: u32| Some(Size { width, height });
    match media_type {
        // The IHDR chunk comes first: width and height after its length
        // and type.
        MediaType::Png => {
            if bytes.get(12..16)? != b"IHDR" {
                return None;
            }
            size(be32(16)?, be32(20)?)
        }
        // The logical screen size after the signature.
        MediaType::Gif => size(u32::from(le16(6)?), u32::from(le16(8)?)),
        MediaType::Webp => match bytes.get(12..16)? {
            // Lossy: 14-bit sizes after the frame tag and start code.
            b"VP8 " => size(u32::from(le16(26)? & 0x3FFF), u32::from(le16(28)? & 0x3FFF)),
            // Lossless: after the signature byte, 14 bits each, minus one.
            b"VP8L" => {
                let bits = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
                size((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
            }
            // Extended: 24 bits each, minus one, after the flags.
            b"VP8X" => size(le24(24)? + 1, le24(27)? + 1),
            _ => None,
        },
        // The first start-of-frame segment, past the others.
        MediaType::Jpeg => {
            let mut at = 2;
            loop {
                if *bytes.get(at)? != 0xFF {
                    return None;
                }
                let marker = *bytes.get(at + 1)?;
                match marker {
                    // Fill bytes before a marker.
                    0xFF => at += 1,
                    // Markers without a length.
                    0x01 | 0xD0..=0xD7 => at += 2,
                    // SOF0 to SOF15, except DHT, JPG and DAC.
                    0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                        return size(u32::from(be16(at + 7)?), u32::from(be16(at + 5)?));
                    }
                    // Image data starts before any frame header: broken.
                    0xD9 | 0xDA => return None,
                    _ => at += 2 + usize::from(be16(at + 2)?),
                }
            }
        }
    }
}

#[cfg(test)]
pub mod testing {
    //! Small valid images for tests.

    /// A PNG of `width` x `height` (header and an empty IDAT; enough for
    /// Junees, which never decodes the pixels).
    pub fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        bytes.extend_from_slice(b"\x00\x00\x00\x00IEND\xaeB`\x82");
        bytes
    }

    /// A JPEG of `width` x `height` with an APP1 segment before its frame
    /// header.
    pub fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x08];
        bytes.extend_from_slice(b"Exif\0\0");
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{jpeg, png};
    use super::*;

    fn gif(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, b';']);
        bytes
    }

    fn webp(chunk: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn formats_are_told_by_their_first_bytes() {
        assert_eq!(MediaType::detect(&png(1, 1)), Some(MediaType::Png));
        assert_eq!(MediaType::detect(&jpeg(1, 1)), Some(MediaType::Jpeg));
        assert_eq!(MediaType::detect(b"GIF87a.."), Some(MediaType::Gif));
        assert_eq!(MediaType::detect(&gif(1, 1)), Some(MediaType::Gif));
        assert_eq!(
            MediaType::detect(&webp(b"VP8 ", &[0; 10])),
            Some(MediaType::Webp)
        );
        for other in [
            &b"hello"[..],
            b"",
            b"\x89PNG\r\n",
            b"RIFF\0\0\0\0WAVE",
            b"%PDF-1.7",
            b"<svg xmlns=",
            b"BM\x36\x00",
        ] {
            assert_eq!(MediaType::detect(other), None, "{other:?}");
        }
        // The name says nothing: a PNG called .txt is a PNG.
        for media in [
            MediaType::Png,
            MediaType::Jpeg,
            MediaType::Gif,
            MediaType::Webp,
        ] {
            assert_eq!(MediaType::parse(media.as_str()), Some(media));
        }
        assert_eq!(MediaType::parse("image/svg+xml"), None);
    }

    #[test]
    fn sizes_come_from_each_header() {
        let at = |w, h| {
            Some(Size {
                width: w,
                height: h,
            })
        };
        assert_eq!(size(MediaType::Png, &png(640, 480)), at(640, 480));
        assert_eq!(size(MediaType::Jpeg, &jpeg(1920, 1080)), at(1920, 1080));
        assert_eq!(size(MediaType::Gif, &gif(300, 200)), at(300, 200));
        // Lossy: frame tag (3), start code (3), then 14-bit sizes with
        // scale bits on top.
        let mut lossy = vec![0x9D, 0x01, 0x2A, 0x9D, 0x01, 0x2A];
        lossy[0] = 0x10;
        lossy.extend_from_slice(&(800u16 | 0x4000).to_le_bytes());
        lossy.extend_from_slice(&600u16.to_le_bytes());
        assert_eq!(size(MediaType::Webp, &webp(b"VP8 ", &lossy)), at(800, 600));
        let bits: u32 = (1023 - 1) | ((767 - 1) << 14);
        let mut lossless = vec![0x2F];
        lossless.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(
            size(MediaType::Webp, &webp(b"VP8L", &lossless)),
            at(1023, 767)
        );
        let mut extended = vec![0; 4];
        extended.extend_from_slice(&[0xFF, 0x0F, 0x00, 0x7F, 0x00, 0x00]);
        assert_eq!(
            size(MediaType::Webp, &webp(b"VP8X", &extended)),
            at(4096, 128)
        );
        // Cut short: no size.
        assert_eq!(size(MediaType::Png, &png(1, 1)[..20]), None);
        assert_eq!(size(MediaType::Jpeg, &jpeg(1, 1)[..12]), None);
        assert_eq!(size(MediaType::Webp, b"RIFF\0\0\0\0WEBPVP8Z"), None);
        // Image data before any frame header.
        assert_eq!(size(MediaType::Jpeg, &[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
    }

    #[test]
    fn the_limits_are_checked() {
        let (media, size) = check(&png(8000, 8000)).unwrap();
        assert_eq!(media, MediaType::Png);
        assert_eq!(size.width, 8000);
        let err = check(&png(8001, 10)).unwrap_err();
        assert!(
            err.contains("8001x10 px") && err.contains("8000x8000"),
            "{err}"
        );
        assert_eq!(check(&png(0, 10)).unwrap_err(), "the image has no pixels");
        assert!(check(b"plain text").unwrap_err().starts_with("not a PNG"));
        assert!(
            check(&png(1, 1)[..20])
                .unwrap_err()
                .contains("cannot read the size")
        );
        let mut big = png(10, 10);
        big.resize(MAX_IMAGE_BYTES as usize + 1, 0);
        let err = check(&big).unwrap_err();
        assert!(
            err.contains("5.0 MB") && err.contains("up to 5 MB"),
            "{err}"
        );
        big.truncate(MAX_IMAGE_BYTES as usize);
        assert!(check(&big).is_ok());
    }

    #[test]
    fn images_go_out_as_base64_and_come_back() {
        let bytes = jpeg(64, 32);
        let (image, size) = Image::from_bytes(&bytes).unwrap();
        assert_eq!(
            size,
            Size {
                width: 64,
                height: 32
            }
        );
        assert_eq!(image.media_type, MediaType::Jpeg);
        let Source::Base64(data) = &image.source else {
            panic!("{image:?}");
        };
        assert_eq!(&**data, STANDARD.encode(&bytes));
        assert_eq!(image.bytes().unwrap(), bytes);
        assert_eq!(image.size(), Some(size));
        assert_eq!(image.describe(), "image/jpeg, 64x32 px");
        assert!(format!("{image:?}").contains("Base64("));
    }

    #[test]
    fn estimates_follow_the_visual_token_formula() {
        let tokens = |bytes: Vec<u8>| Image::from_bytes(&bytes).unwrap().0.estimate_tokens();
        // The table of the vision docs, high-resolution column.
        assert_eq!(tokens(png(200, 200)), 64);
        assert_eq!(tokens(png(1000, 1000)), 1296);
        assert_eq!(tokens(png(1920, 1080)), 2691);
        assert_eq!(tokens(png(2000, 1500)), 3888);
        // 3840x2160 is scaled to 2576x1449 and capped.
        assert_eq!(tokens(png(3840, 2160)), 4784);
        assert_eq!(tokens(png(1, 1)), 1);
        // Unknown size: the most an image costs.
        let stored = Image {
            media_type: MediaType::Png,
            source: Source::Stored {
                sha256: "ab".into(),
            },
        };
        assert_eq!(stored.estimate_tokens(), MAX_IMAGE_TOKENS);
        assert_eq!(stored.describe(), "image/png");
    }

    #[test]
    fn loads_files_and_says_what_is_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        std::fs::write(&path, png(10, 20)).unwrap();
        let (image, line) = load_file(&path).unwrap();
        assert_eq!(image.media_type, MediaType::Png);
        assert!(
            line.ends_with("shot.png (image/png, 10x20 px, 45 B)"),
            "{line}"
        );

        let text = dir.path().join("notes.png");
        std::fs::write(&text, "not an image").unwrap();
        assert!(
            load_file(&text)
                .unwrap_err()
                .contains("notes.png: not a PNG")
        );
        let missing = load_file(&dir.path().join("nope.png")).unwrap_err();
        assert!(missing.starts_with("cannot read") && missing.contains("nope.png"));
        assert!(
            load_file(dir.path())
                .unwrap_err()
                .contains("not a regular file")
        );
        let big = dir.path().join("big.png");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        assert!(load_file(&big).unwrap_err().contains("up to 5 MB"));
    }

    #[test]
    fn paths_resolve_like_a_shell_would() {
        let cwd = Path::new("/work");
        let home = Some(Path::new("/home/j"));
        assert_eq!(resolve_path("a.png", cwd, home), Path::new("/work/a.png"));
        assert_eq!(
            resolve_path("/tmp/a.png", cwd, home),
            Path::new("/tmp/a.png")
        );
        assert_eq!(
            resolve_path("~/a.png", cwd, home),
            Path::new("/home/j/a.png")
        );
        assert_eq!(resolve_path("~", cwd, home), Path::new("/home/j"));
        assert_eq!(
            resolve_path("~x/a.png", cwd, home),
            Path::new("/work/~x/a.png")
        );
        assert_eq!(
            resolve_path("~/a.png", cwd, None),
            Path::new("/work/~/a.png")
        );
    }

    #[test]
    fn byte_counts_read_well() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
