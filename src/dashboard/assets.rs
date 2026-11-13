//! The files of the page (`web/dashboard/`, without `dev/`), built into
//! the binary. `build.rs` lists them, so a file the frontend adds is
//! served as it is; its content type follows the extension.

/// One file of the page.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// The path it is served at.
    pub path: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

/// Every file served; `/` is `index.html`.
pub const ASSETS: &[Asset] = include!(concat!(env!("OUT_DIR"), "/dashboard_assets.rs"));

/// The file served at `path`.
pub fn find(path: &str) -> Option<&'static Asset> {
    let path = if path == "/" { "/index.html" } else { path };
    ASSETS.iter().find(|asset| asset.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = "text/html; charset=utf-8";
    const CSS: &str = "text/css; charset=utf-8";
    const JS: &str = "text/javascript; charset=utf-8";

    #[test]
    fn serves_the_page_files_only() {
        assert_eq!(find("/").unwrap().path, "/index.html");
        assert_eq!(find("/app.js").unwrap().content_type, JS);
        assert_eq!(find("/style.css").unwrap().content_type, CSS);
        for path in [
            "/dev/fixtures/status.json",
            "/dev/serve.py",
            "/../Cargo.toml",
            "/index.htm",
            "",
        ] {
            assert!(find(path).is_none(), "{path}");
        }
        assert!(ASSETS.iter().all(|asset| !asset.path.starts_with("/dev/")));
    }

    #[test]
    fn every_file_on_disk_is_served_with_its_type() {
        let page = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/dashboard");
        for entry in std::fs::read_dir(&page).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if !entry.file_type().unwrap().is_file() || name.starts_with('.') {
                continue;
            }
            let asset = find(&format!("/{name}")).unwrap_or_else(|| panic!("{name} is not served"));
            assert_eq!(asset.bytes, std::fs::read(entry.path()).unwrap(), "{name}");
            let expected = match name.rsplit_once('.').map(|(_, ext)| ext) {
                Some("html") => HTML,
                Some("css") => CSS,
                Some("js") => JS,
                _ => continue,
            };
            assert_eq!(asset.content_type, expected, "{name}");
        }
    }

    /// The local module paths a script imports: `from "./x.js"`,
    /// `import "./x.js"` and `import("./x.js")`.
    fn imports(script: &str) -> Vec<String> {
        let mut found = Vec::new();
        for marker in ["from", "import", "import("] {
            let mut rest = script;
            while let Some(at) = rest.find(marker) {
                rest = &rest[at + marker.len()..];
                let quoted = rest.trim_start();
                let Some(quote) = quoted.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
                    continue;
                };
                let body = &quoted[1..];
                if let Some(end) = body.find(quote) {
                    let target = &body[..end];
                    let local = target.starts_with("./")
                        || target.starts_with("../")
                        || target.starts_with('/');
                    if local && !found.iter().any(|seen| seen == target) {
                        found.push(target.to_string());
                    }
                }
            }
        }
        found
    }

    /// `target` as imported by the asset at `from`, as a served path.
    fn resolve(from: &str, target: &str) -> String {
        if target.starts_with('/') {
            return target.to_string();
        }
        let mut parts: Vec<&str> = from.split('/').filter(|part| !part.is_empty()).collect();
        parts.pop();
        for part in target.split('/') {
            match part {
                "." | "" => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        format!("/{}", parts.join("/"))
    }

    #[test]
    fn finds_imports() {
        let script = "import { a } from \"./api.js\";\nimport './dom.js';\n\
                      const v = await import(\"./view-x.js\");\n\
                      import x from 'https://cdn.example/x.js';\n// from here on\n";
        assert_eq!(imports(script), ["./api.js", "./dom.js", "./view-x.js"]);
        assert_eq!(resolve("/app.js", "./api.js"), "/api.js");
        assert_eq!(resolve("/views/a.js", "../dom.js"), "/dom.js");
    }

    #[test]
    fn every_import_and_link_of_the_page_is_served() {
        let mut checked = 0;
        for asset in ASSETS.iter().filter(|asset| asset.content_type == JS) {
            let script = std::str::from_utf8(asset.bytes).unwrap();
            for target in imports(script) {
                let path = resolve(asset.path, &target);
                let served = find(&path).unwrap_or_else(|| {
                    panic!("{} imports {target}, which is not served", asset.path)
                });
                assert_eq!(served.content_type, JS, "{path}");
                checked += 1;
            }
        }
        let index = std::str::from_utf8(find("/").unwrap().bytes).unwrap();
        for attribute in ["src=\"", "href=\""] {
            for part in index.split(attribute).skip(1) {
                let target = part.split('"').next().unwrap();
                if target.starts_with('/') || target.starts_with("./") {
                    let path = resolve("/index.html", target);
                    assert!(
                        find(&path).is_some(),
                        "index.html links {target}, which is not served"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "no link was checked");
    }
}
