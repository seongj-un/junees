//! Which files of `web/dashboard/` are served, and as what. `build.rs`
//! includes this file (`#[path]`) to list the page's files; the lib
//! compiles it for its tests only.

/// The content type of each extension that may be served.
pub const TYPES: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("json", "application/json; charset=utf-8"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("ico", "image/x-icon"),
    ("webp", "image/webp"),
    ("woff2", "font/woff2"),
    ("txt", "text/plain; charset=utf-8"),
];

/// The endings of editor and merge leftovers (`app.js~`, `app.js.orig`,
/// `app.js.rej`, `app.js.bak`, `app.js.swp`), which are never served.
pub const BACKUP_ENDINGS: &[&str] = &["~", ".orig", ".rej", ".bak", ".swp"];

/// What to do with a file of the page, by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Served with this content type.
    Serve(&'static str),
    /// Left out: a hidden file or an editor or merge leftover.
    Skip,
    /// An extension with no content type: the build stops.
    Unknown,
}

/// What to do with the file called `name` (no folder).
pub fn judge(name: &str) -> Verdict {
    if name.starts_with('.') || is_backup(name) {
        return Verdict::Skip;
    }
    let ext = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    TYPES
        .iter()
        .find(|(known, _)| *known == ext)
        .map_or(Verdict::Unknown, |(_, content_type)| {
            Verdict::Serve(content_type)
        })
}

/// Whether `name` is an editor or merge leftover ([`BACKUP_ENDINGS`]).
pub fn is_backup(name: &str) -> bool {
    BACKUP_ENDINGS.iter().any(|ending| name.ends_with(ending))
}

/// Whether `name` (a file or folder of the page) may be part of a URL as
/// it is: letters, digits, `-`, `_` and `.`.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// Why the build stops at `path`, whose name [`judge`]d
/// [`Verdict::Unknown`], and how to go on.
pub fn unknown_type_message(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let what = match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => format!("`.{ext}` files have"),
        _ => "a file without an extension has".to_string(),
    };
    format!(
        "{path}: {what} no content type, so the dashboard cannot serve it. If it is a \
         leftover, delete it; if it is not part of the page, move it to web/dashboard/dev/; \
         if it is, add its type to TYPES in src/dashboard/page_files.rs. (Editor and merge \
         leftovers ending in {} are skipped.)",
        BACKUP_ENDINGS.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_files_are_served_with_their_type() {
        assert_eq!(
            judge("app.js"),
            Verdict::Serve("text/javascript; charset=utf-8")
        );
        assert_eq!(
            judge("view-status.js"),
            Verdict::Serve("text/javascript; charset=utf-8")
        );
        assert_eq!(
            judge("index.html"),
            Verdict::Serve("text/html; charset=utf-8")
        );
        assert_eq!(
            judge("style.css"),
            Verdict::Serve("text/css; charset=utf-8")
        );
        assert_eq!(judge("logo.svg"), Verdict::Serve("image/svg+xml"));
    }

    #[test]
    fn leftovers_and_hidden_files_are_skipped() {
        for name in [
            "app.js~",
            "app.js.orig",
            "index.html.rej",
            "style.css.bak",
            "app.js.swp",
            ".app.js.swp",
            ".DS_Store",
            ".gitkeep",
            "notes.md~",
        ] {
            assert_eq!(judge(name), Verdict::Skip, "{name}");
        }
    }

    #[test]
    fn other_extensions_stop_the_build() {
        for name in [
            "README.md",
            "app.ts",
            "Makefile",
            "app.js.old",
            "index.htm",
            "x.",
        ] {
            assert_eq!(judge(name), Verdict::Unknown, "{name}");
        }
    }

    #[test]
    fn the_message_says_why_and_how_to_go_on() {
        let message = unknown_type_message("/repo/web/dashboard/README.md");
        assert!(
            message.starts_with("/repo/web/dashboard/README.md: `.md` files have no content type"),
            "{message}"
        );
        for part in [
            "delete it",
            "web/dashboard/dev/",
            "TYPES in src/dashboard/page_files.rs",
            "~, .orig, .rej, .bak, .swp",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }
        let message = unknown_type_message("web/dashboard/Makefile");
        assert!(
            message.contains("a file without an extension has no content type"),
            "{message}"
        );
    }

    #[test]
    fn names_in_urls_are_plain() {
        for name in ["app.js", "view-status.js", "a_b.css", "img"] {
            assert!(plain_name(name), "{name}");
        }
        for name in ["", "a b.js", "a%20.js", "é.js", "a?.js", "a#.js"] {
            assert!(!plain_name(name), "{name}");
        }
    }
}
