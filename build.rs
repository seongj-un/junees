//! Lists the files of the dashboard page (`web/dashboard/`, without its
//! `dev/` folder of fixtures and dev tools) for `src/dashboard/assets.rs`,
//! which builds them into the binary with `include_bytes!`. A file the
//! frontend adds is served without touching the Rust code; one whose
//! extension has no content type stops the build, so it is never served
//! as the wrong type or silently left out. Editor and merge leftovers
//! (`app.js~`, `.orig`) are skipped. The rules are in
//! `src/dashboard/page_files.rs`, which the lib tests.

#[path = "src/dashboard/page_files.rs"]
mod page_files;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Where the page's files are, from the crate root.
const PAGE_DIR: &str = "web/dashboard";

/// Folders of `PAGE_DIR` that are never served.
const NOT_SERVED: &[&str] = &["dev"];

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let page = root.join(PAGE_DIR);
    // A directory makes cargo look at every file under it.
    println!("cargo:rerun-if-changed={PAGE_DIR}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/dashboard/page_files.rs");

    let mut files = Vec::new();
    collect(&page, &page, &mut files);
    files.sort();

    let mut out = String::from("&[\n");
    for (url, path, content_type) in &files {
        let path = path.to_str().expect("a page file's path is not UTF-8");
        writeln!(
            out,
            "    Asset {{ path: {url:?}, content_type: {content_type:?}, bytes: include_bytes!({path:?}) }},"
        )
        .unwrap();
    }
    out.push_str("]\n");
    let dest =
        PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("dashboard_assets.rs");
    std::fs::write(&dest, out).expect("cannot write the dashboard asset list");
}

/// Every file under `dir` to serve as (URL path, file path, content
/// type), skipping hidden entries, leftovers ([`page_files::judge`]) and
/// the [`NOT_SERVED`] folders at the top. A file of unknown type stops the
/// build.
fn collect(page: &Path, dir: &Path, files: &mut Vec<(String, PathBuf, &'static str)>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()));
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
            let content_type = match page_files::judge(name) {
                page_files::Verdict::Serve(content_type) => content_type,
                page_files::Verdict::Skip => continue,
                page_files::Verdict::Unknown => {
                    panic!(
                        "{}",
                        page_files::unknown_type_message(&path.display().to_string())
                    )
                }
            };
            let relative = path.strip_prefix(page).expect("under the page folder");
            let url: Vec<&str> = relative
                .components()
                .map(|part| part.as_os_str().to_str().expect("UTF-8"))
                .collect();
            for part in &url {
                assert!(
                    page_files::plain_name(part),
                    "{}: page file and folder names may hold only letters, digits, `-`, `_` \
                     and `.`; rename it",
                    path.display()
                );
            }
            files.push((format!("/{}", url.join("/")), path, content_type));
        }
    }
}
