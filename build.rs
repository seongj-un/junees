//! Lists the files of the dashboard page (`web/dashboard/`, without its
//! `dev/` folder of fixtures and dev tools) for `src/dashboard/assets.rs`,
//! which builds them into the binary with `include_bytes!`. A file the
//! frontend adds is served without touching the Rust code; one whose
//! extension has no content type here stops the build, so it is never
//! served as the wrong type or silently left out.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Where the page's files are, from the crate root.
const PAGE_DIR: &str = "web/dashboard";

/// Folders of `PAGE_DIR` that are never served.
const NOT_SERVED: &[&str] = &["dev"];

/// The content type of each extension that may be served.
const TYPES: &[(&str, &str)] = &[
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

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let page = root.join(PAGE_DIR);
    // A directory makes cargo look at every file under it.
    println!("cargo:rerun-if-changed={PAGE_DIR}");
    println!("cargo:rerun-if-changed=build.rs");

    let mut files = Vec::new();
    collect(&page, &page, &mut files);
    files.sort();

    let mut out = String::from("&[\n");
    for (url, path) in &files {
        let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        let Some((_, content_type)) = TYPES.iter().find(|(known, _)| *known == ext) else {
            panic!(
                "{}: no content type for `.{ext}` files; add one to TYPES in build.rs, \
                 or move the file to {PAGE_DIR}/dev/ if it is not served",
                path.display()
            );
        };
        let path = path.to_str().expect("a page file's path is not UTF-8");
        writeln!(
            out,
            "    Asset {{ path: {url:?}, content_type: {content_type:?}, bytes: include_bytes!({path:?}) }},"
        )
        .unwrap();
    }
    out.push_str("]\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("dashboard_assets.rs");
    std::fs::write(&dest, out).expect("cannot write the dashboard asset list");
}

/// Every file under `dir` as (URL path, file path), skipping hidden
/// entries and the [`NOT_SERVED`] folders at the top.
fn collect(page: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()));
    for entry in entries {
        let entry = entry.expect("cannot read a page folder entry");
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().expect("a page file's name is not UTF-8");
        if name.starts_with('.') {
            continue;
        }
        let kind = entry.file_type().expect("cannot read a page entry's type");
        if kind.is_dir() {
            if dir == page && NOT_SERVED.contains(&name) {
                continue;
            }
            collect(page, &path, files);
        } else if kind.is_file() {
            let relative = path.strip_prefix(page).expect("under the page folder");
            let url: Vec<&str> = relative
                .components()
                .map(|part| part.as_os_str().to_str().expect("UTF-8"))
                .collect();
            for part in &url {
                assert!(
                    part.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                    "{}: page file names may hold only letters, digits, `-`, `_` and `.`",
                    path.display()
                );
            }
            files.push((format!("/{}", url.join("/")), path));
        }
    }
}
