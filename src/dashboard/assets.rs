//! The files of the page (`web/dashboard/`), built into the binary. A
//! new file there is served only once it is listed in [`ASSETS`].

/// One file of the page.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// The path it is served at.
    pub path: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

/// Every file served; `/` is `index.html`.
pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/index.html",
        content_type: HTML,
        bytes: include_bytes!("../../web/dashboard/index.html"),
    },
    Asset {
        path: "/style.css",
        content_type: CSS,
        bytes: include_bytes!("../../web/dashboard/style.css"),
    },
    Asset {
        path: "/app.js",
        content_type: JS,
        bytes: include_bytes!("../../web/dashboard/app.js"),
    },
];

/// The file served at `path`.
pub fn find(path: &str) -> Option<&'static Asset> {
    let path = if path == "/" { "/index.html" } else { path };
    ASSETS.iter().find(|asset| asset.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_listed_files_only() {
        assert_eq!(find("/").unwrap().path, "/index.html");
        assert_eq!(find("/app.js").unwrap().content_type, JS);
        assert_eq!(find("/style.css").unwrap().content_type, CSS);
        for path in [
            "/dev/fixtures/status.json",
            "/../Cargo.toml",
            "/index.htm",
            "",
        ] {
            assert!(find(path).is_none(), "{path}");
        }
    }

    #[test]
    fn content_types_follow_the_extension() {
        for asset in ASSETS {
            let expected = match asset.path.rsplit_once('.').map(|(_, ext)| ext) {
                Some("html") => HTML,
                Some("css") => CSS,
                Some("js") => JS,
                other => panic!("no content type for {other:?}"),
            };
            assert_eq!(asset.content_type, expected, "{}", asset.path);
        }
    }
}
