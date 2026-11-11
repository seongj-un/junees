//! How session journals keep images: each image once, as a file of the
//! session's own folder, `<journal without .jsonl>/images/<sha256>.<ext>`
//! (for a session, `$JUNEES_HOME/sessions/<id>/images/`), and in the
//! journal line a block whose source names that file
//! ([`crate::message::STORED_SOURCE`]). Base64 in every line would grow
//! the journal by a third more than the image at each mention, and
//! session search and `junees usage` read every line.
//!
//! [`crate::session::Session::load`] reads the files back into the blocks,
//! so a resumed conversation sends the same images. A file that is gone or
//! changed becomes a short text saying so: the conversation stays usable.
//! The folder is 0700 and the files 0600, like the journal: they can show
//! anything the model read.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::{Image, Source};
use crate::message::{ContentBlock, Message};
use crate::session::Entry;

/// The image folder of the journal at `journal`.
pub fn dir_of(journal: &Path) -> PathBuf {
    journal.with_extension("").join("images")
}

/// The SHA-256 of `bytes` in lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The entry as the journal writes it: every inline image saved to `dir`
/// and named by its file instead. `None` when the entry holds no inline
/// image, so it is written as it is.
pub fn stored_entry(entry: &Entry, dir: &Path) -> io::Result<Option<Entry>> {
    let content = match entry {
        Entry::Message(message) => &message.content,
        Entry::AppendContent { content } => content,
        _ => return Ok(None),
    };
    let inline = |image: &Image| matches!(image.source, Source::Base64(_));
    if !content
        .iter()
        .any(|block| block.images().iter().any(inline))
    {
        return Ok(None);
    }
    let mut stored = Vec::with_capacity(content.len());
    for block in content {
        stored.push(match block {
            ContentBlock::Image(image) => ContentBlock::Image(store(image, dir)?),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
            } => ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
                images: images
                    .iter()
                    .map(|image| store(image, dir))
                    .collect::<io::Result<_>>()?,
            },
            other => other.clone(),
        });
    }
    Ok(Some(match entry {
        Entry::Message(message) => Entry::Message(Message {
            role: message.role,
            content: stored,
        }),
        _ => Entry::AppendContent { content: stored },
    }))
}

/// Saves an inline image to `dir` (once: an existing file of the same
/// hash is the same image) and returns the block naming it.
fn store(image: &Image, dir: &Path) -> io::Result<Image> {
    let Source::Base64(data) = &image.source else {
        return Ok(image.clone());
    };
    let bytes = STANDARD
        .decode(data.as_bytes())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("image: {err}")))?;
    let sha256 = sha256_hex(&bytes);
    let path = file_path(dir, &sha256, image);
    if !fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64) {
        write_file(dir, &path, &bytes)?;
    }
    Ok(Image {
        media_type: image.media_type,
        source: Source::Stored { sha256 },
    })
}

fn file_path(dir: &Path, sha256: &str, image: &Image) -> PathBuf {
    dir.join(format!("{sha256}.{}", image.media_type.extension()))
}

/// Writes `bytes` to `path` through a temporary file in `dir`, so a crash
/// never leaves a cut-off image under the final name.
fn write_file(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// The entry with every image named by a file of `dir` read back. One
/// that cannot be read (deleted, or changed since: its hash no longer
/// matches) becomes a note in text.
pub fn resolve_entry(entry: Entry, dir: &Path) -> Entry {
    let stored = |block: &ContentBlock| {
        block
            .images()
            .iter()
            .any(|image| matches!(image.source, Source::Stored { .. }))
    };
    match entry {
        Entry::Message(Message { role, content }) if content.iter().any(stored) => {
            Entry::Message(Message {
                role,
                content: resolve(content, dir),
            })
        }
        Entry::AppendContent { content } if content.iter().any(stored) => Entry::AppendContent {
            content: resolve(content, dir),
        },
        entry => entry,
    }
}

fn resolve(content: Vec<ContentBlock>, dir: &Path) -> Vec<ContentBlock> {
    content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Image(image) => match load(&image, dir) {
                Ok(image) => ContentBlock::Image(image),
                Err(note) => ContentBlock::text(note),
            },
            ContentBlock::ToolResult {
                tool_use_id,
                mut content,
                is_error,
                images,
            } => {
                let mut kept = Vec::with_capacity(images.len());
                for image in images {
                    match load(&image, dir) {
                        Ok(image) => kept.push(image),
                        Err(note) => {
                            content.push('\n');
                            content.push_str(&note);
                        }
                    }
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    images: kept,
                }
            }
            other => other,
        })
        .collect()
}

/// Reads a stored image back, or says why it cannot.
fn load(image: &Image, dir: &Path) -> Result<Image, String> {
    let Source::Stored { sha256 } = &image.source else {
        return Ok(image.clone());
    };
    let path = file_path(dir, sha256, image);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let bytes = fs::read(&path)
        .map_err(|err| format!("[image {name} of this session could not be read: {err}]"))?;
    if sha256_hex(&bytes) != *sha256 {
        return Err(format!(
            "[image {name} of this session changed on disk since it was saved; left out]"
        ));
    }
    Ok(Image {
        media_type: image.media_type,
        source: Source::Base64(STANDARD.encode(&bytes).into()),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::images::testing::png;
    use crate::message::Role;

    fn image(width: u32) -> Image {
        Image::from_bytes(&png(width, 7)).unwrap().0
    }

    fn message(content: Vec<ContentBlock>) -> Entry {
        Entry::Message(Message {
            role: Role::User,
            content,
        })
    }

    #[test]
    fn images_are_saved_once_and_read_back() {
        let home = tempfile::tempdir().unwrap();
        let dir = dir_of(&home.path().join("sessions/20261111-000000-abcd.jsonl"));
        assert!(dir.ends_with("sessions/20261111-000000-abcd/images"));
        let first = image(5);
        let entry = message(vec![
            ContentBlock::Image(first.clone()),
            ContentBlock::text("what is this?"),
            ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "Image: a.png".into(),
                is_error: false,
                images: vec![first.clone(), image(6)],
            },
        ]);
        let stored = stored_entry(&entry, &dir).unwrap().unwrap();
        let line = serde_json::to_string(&stored).unwrap();
        assert!(!line.contains("\"base64\""), "{line}");
        assert_eq!(line.matches("\"junees_file\"").count(), 3);
        let sha = sha256_hex(&png(5, 7));
        assert!(line.contains(&sha));

        // Two files: the same image twice is one file.
        let mut files: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&dir.join(format!("{sha}.png"))));
        for file in &files {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read(dir.join(format!("{sha}.png"))).unwrap(), png(5, 7));

        // Read back through the journal's own parsing.
        let parsed: Entry = serde_json::from_str(&line).unwrap();
        assert_eq!(resolve_entry(parsed, &dir), entry);
        // Nothing inline: nothing to store.
        assert_eq!(stored_entry(&stored, &dir).unwrap(), None);
        assert_eq!(
            stored_entry(&message(vec![ContentBlock::text("hi")]), &dir).unwrap(),
            None
        );
        let content = Entry::AppendContent {
            content: vec![ContentBlock::Image(image(9))],
        };
        assert!(matches!(
            stored_entry(&content, &dir).unwrap(),
            Some(Entry::AppendContent { .. })
        ));
    }

    #[test]
    fn missing_or_changed_files_become_notes() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("images");
        let entry = message(vec![
            ContentBlock::Image(image(5)),
            ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "Image: b.png".into(),
                is_error: false,
                images: vec![image(6)],
            },
        ]);
        let stored = stored_entry(&entry, &dir).unwrap().unwrap();
        let sha5 = sha256_hex(&png(5, 7));
        let sha6 = sha256_hex(&png(6, 7));
        fs::remove_file(dir.join(format!("{sha5}.png"))).unwrap();
        fs::write(dir.join(format!("{sha6}.png")), png(1, 1)).unwrap();
        let Entry::Message(message) = resolve_entry(stored, &dir) else {
            panic!();
        };
        let ContentBlock::Text { text, .. } = &message.content[0] else {
            panic!("{:?}", message.content[0]);
        };
        assert!(
            text.starts_with(&format!(
                "[image {sha5}.png of this session could not be read"
            )),
            "{text}"
        );
        let ContentBlock::ToolResult {
            content, images, ..
        } = &message.content[1]
        else {
            panic!();
        };
        assert!(images.is_empty());
        assert!(
            content.starts_with("Image: b.png\n[image ") && content.contains("changed on disk"),
            "{content}"
        );
    }

    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let home = tempfile::tempdir().unwrap();
        // A file where the folder should be.
        let dir = home.path().join("images");
        fs::write(&dir, "").unwrap();
        let entry = message(vec![ContentBlock::Image(image(5))]);
        assert!(stored_entry(&entry, &dir).is_err());
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }
}
