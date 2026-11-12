//! Tests of `junees update` against a fake release server on loopback.
//! The binary replaced is always a fake one in a temporary folder.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

const TARGET: &str = "x86_64-unknown-linux-musl";

/// One answer of the fake server.
#[derive(Clone)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Leave out Content-Length (the body ends when the connection does).
    no_length: bool,
}

impl Answer {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            no_length: false,
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            ..Self::ok("")
        }
    }

    fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".into(), location.into())],
            ..Self::ok("")
        }
    }
}

/// A release server on 127.0.0.1: answers by path (404 for the rest)
/// and records the paths asked for.
struct Server {
    base: String,
    port: u16,
    paths: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start(answers: Vec<(&str, Answer)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let answers: HashMap<String, Answer> = answers
            .into_iter()
            .map(|(path, answer)| (path.to_string(), answer))
            .collect();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let seen = paths.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                seen.lock().unwrap().push(path.clone());
                let answer = answers
                    .get(&path)
                    .cloned()
                    .unwrap_or_else(|| Answer::status(404));
                let mut out = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", answer.status);
                for (name, value) in &answer.headers {
                    out.push_str(&format!("{name}: {value}\r\n"));
                }
                if !answer.no_length {
                    out.push_str(&format!("Content-Length: {}\r\n", answer.body.len()));
                }
                out.push_str("\r\n");
                let mut bytes = out.into_bytes();
                bytes.extend_from_slice(&answer.body);
                let _ = socket.write_all(&bytes).await;
                let _ = socket.shutdown().await;
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}/releases"),
            port,
            paths,
            task,
        }
    }

    fn client(&self) -> Client {
        Client::new(Source::loopback(&self.base).unwrap()).unwrap()
    }

    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

/// A release archive like the workflow's: `<stem>/junees`, LICENSE and
/// README.md in a gzipped tar.
fn archive(version: &str, binary: &[u8]) -> Vec<u8> {
    archive_for(version, TARGET, binary)
}

fn archive_for(version: &str, target: &str, binary: &[u8]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let stem = install::stem(&v(version), target);
    let folder = dir.path().join(&stem);
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("junees"), binary).unwrap();
    std::fs::set_permissions(
        folder.join("junees"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(folder.join("LICENSE"), "MIT").unwrap();
    std::fs::write(folder.join("README.md"), "# Junees").unwrap();
    let out = dir.path().join("out.tar.gz");
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&out)
        .arg("-C")
        .arg(dir.path())
        .arg(&stem)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read(out).unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The answers of a server that has release `version` with `binary`, its
/// SHA256SUMS saying `sum` for the archive (`None`: the right one).
fn release(version: &str, binary: &[u8], sum: Option<&str>) -> Vec<(String, Answer)> {
    release_for(version, TARGET, binary, sum)
}

/// [`release`] with an archive for `target`. Also answers
/// `latest/download/SHA256SUMS` as GitHub does (the redirect followed),
/// for install.sh.
fn release_for(
    version: &str,
    target: &str,
    binary: &[u8],
    sum: Option<&str>,
) -> Vec<(String, Answer)> {
    let tar = archive_for(version, target, binary);
    let name = format!("{}.tar.gz", install::stem(&v(version), target));
    let sum = sum.map_or_else(|| sha256_hex(&tar), str::to_string);
    let sums = format!(
        "{}  junees-{version}-aarch64-apple-darwin.tar.gz\n{sum}  {name}\n",
        "0".repeat(64)
    );
    vec![
        (
            "/releases/latest".to_string(),
            Answer::redirect(&format!("/seongj-un/junees/releases/tag/v{version}")),
        ),
        (
            format!("/releases/download/v{version}/SHA256SUMS"),
            Answer::ok(sums.clone()),
        ),
        (
            format!("/releases/download/v{version}/{name}"),
            Answer::ok(tar),
        ),
        (
            "/releases/latest/download/SHA256SUMS".to_string(),
            Answer::ok(sums),
        ),
    ]
}

async fn start(answers: Vec<(String, Answer)>) -> Server {
    Server::start(
        answers
            .iter()
            .map(|(path, answer)| (path.as_str(), answer.clone()))
            .collect(),
    )
    .await
}

/// A fake installed junees: `bin/junees` (0755, "old binary") and a
/// link `link/junees` to it, as `~/.local/bin` often holds.
struct Installed {
    dir: tempfile::TempDir,
}

impl Installed {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        std::fs::create_dir(dir.path().join("link")).unwrap();
        let real = dir.path().join("bin/junees");
        std::fs::write(&real, "old binary").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link/junees")).unwrap();
        Self { dir }
    }

    fn real(&self) -> PathBuf {
        self.dir.path().join("bin/junees").canonicalize().unwrap()
    }

    fn place(&self, current: &str) -> Place {
        Place {
            exe: self.dir.path().join("link/junees"),
            current: v(current),
            target: Some(TARGET),
            home: None,
            cargo_bin: None,
        }
    }

    fn contents(&self) -> String {
        std::fs::read_to_string(self.real()).unwrap()
    }

    /// The files in `bin`, to see no temporary file is left.
    fn bin_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.path().join("bin"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

async fn update(
    server: &Server,
    options: &Options,
    place: &Place,
) -> (Result<String>, Vec<String>) {
    let mut said = Vec::new();
    let result = run(options, &server.client(), place, &mut |line| {
        said.push(line.to_string())
    })
    .await;
    (result, said)
}

#[test]
fn parses_its_arguments() {
    let parse = |args: &[&str]| parse(args.iter().map(|arg| arg.to_string()));
    assert_eq!(parse(&[]).unwrap(), Some(Options::default()));
    assert_eq!(
        parse(&["--check"]).unwrap(),
        Some(Options {
            check: true,
            version: None
        })
    );
    assert_eq!(
        parse(&["--version", "v0.3.1"]).unwrap(),
        Some(Options {
            check: false,
            version: Some(v("0.3.1"))
        })
    );
    assert_eq!(parse(&["--check", "-h"]).unwrap(), None);
    for (args, part) in [
        (&["--version"][..], "--version needs a version"),
        (&["--version", "latest"], "is not a release version"),
        (&["--check", "--version", "0.2.0"], "do not go together"),
        (&["--force"], "unknown argument `--force`"),
    ] {
        let err = format!("{:#}", parse(args).unwrap_err());
        assert!(err.contains(part), "{args:?}: {err}");
    }
}

#[test]
fn says_how_the_latest_release_compares() {
    let current = v("0.2.0");
    assert_eq!(
        compare(&current, Some(&v("0.10.0"))),
        "junees 0.10.0 is out (this is 0.2.0): run `junees update`"
    );
    assert_eq!(
        compare(&current, Some(&v("0.2.0"))),
        "junees 0.2.0 is the latest release"
    );
    assert_eq!(
        compare(&current, Some(&v("0.2.0-rc.1"))),
        "this junees (0.2.0) is newer than the latest release (0.2.0-rc.1)"
    );
    assert_eq!(
        compare(&current, None),
        "no release of Junees is published yet (this is 0.2.0)"
    );
}

#[test]
fn knows_the_release_build_of_each_platform() {
    use install::target_for;
    assert_eq!(
        target_for("linux", "x86_64"),
        Some("x86_64-unknown-linux-musl")
    );
    assert_eq!(
        target_for("linux", "aarch64"),
        Some("aarch64-unknown-linux-musl")
    );
    assert_eq!(target_for("macos", "x86_64"), Some("x86_64-apple-darwin"));
    assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
    assert_eq!(target_for("linux", "riscv64"), None);
    assert_eq!(target_for("freebsd", "x86_64"), None);
    assert_eq!(
        install::stem(&v("0.1.0"), "x86_64-apple-darwin"),
        "junees-0.1.0-x86_64-apple-darwin"
    );
}

#[test]
fn reads_sha256sums() {
    let a = "ab".repeat(32);
    let b = "CD".repeat(32);
    let sums = format!("{a}  junees-0.1.0-x.tar.gz\n{b} *junees-0.1.0-y.tar.gz\n");
    assert_eq!(
        install::checksum_of(sums.as_bytes(), "junees-0.1.0-x.tar.gz").unwrap(),
        [0xab; 32]
    );
    assert_eq!(
        install::checksum_of(sums.as_bytes(), "junees-0.1.0-y.tar.gz").unwrap(),
        [0xcd; 32]
    );
    let err = install::checksum_of(sums.as_bytes(), "junees-0.1.0-z.tar.gz").unwrap_err();
    assert!(err.to_string().contains("lists no junees-0.1.0-z"), "{err}");
    let err = install::checksum_of(b"abc  f.tar.gz\n", "f.tar.gz").unwrap_err();
    assert!(err.to_string().contains("broken line"), "{err}");
}

#[test]
fn only_github_release_hosts_or_the_loopback_test_server() {
    use reqwest::Url;
    let github = Source::github();
    let url = |text: &str| Url::parse(text).unwrap();
    for allowed in [
        "https://github.com/seongj-un/junees/releases/download/v0.1.0/SHA256SUMS",
        "https://release-assets.githubusercontent.com/github-production-release-asset/1/2",
        "https://objects.githubusercontent.com/x",
    ] {
        assert!(github.allows(&url(allowed)), "{allowed}");
    }
    for refused in [
        "http://github.com/seongj-un/junees/releases",
        "https://github.com:8443/x",
        "https://evil.example/x",
        "https://github.com.evil.example/x",
        "https://user:pass@github.com/x",
        "http://127.0.0.1:8080/releases/x",
    ] {
        assert!(!github.allows(&url(refused)), "{refused}");
    }
    let local = Source::loopback("http://127.0.0.1:8080/releases/").unwrap();
    assert!(local.allows(&url("http://127.0.0.1:8080/other")));
    assert!(!local.allows(&url("http://127.0.0.1:8081/releases/x")));
    assert!(!local.allows(&url("http://localhost:8080/releases/x")));
    assert!(!local.allows(&url("https://github.com/x")));
    for bad in [
        "https://github.com/seongj-un/junees/releases",
        "http://10.0.0.1/releases",
        "ftp://127.0.0.1/releases",
        "http://127.0.0.1/releases?x=1",
        "http://u:p@127.0.0.1/releases",
        "not a url",
    ] {
        assert!(Source::loopback(bad).is_err(), "{bad}");
    }
    let lookup =
        |value: &'static str| move |key: &str| (key == BASE_ENV).then(|| value.to_string());
    assert!(
        !Source::from_lookup(&lookup(""))
            .unwrap()
            .describe()
            .contains("test")
    );
    assert!(Source::from_lookup(&lookup("http://[::1]:9/releases")).is_ok());
    assert!(Source::from_lookup(&lookup("https://example.com/releases")).is_err());
}

#[tokio::test]
async fn reads_the_latest_release_from_the_redirect() {
    let server = Server::start(vec![
        (
            "/a/releases/latest",
            Answer::redirect("https://github.com/seongj-un/junees/releases/tag/v0.4.2"),
        ),
        // No release yet: GitHub sends the release list.
        (
            "/b/releases/latest",
            Answer::redirect("/seongj-un/junees/releases"),
        ),
        ("/c/releases/latest", Answer::status(500)),
        (
            "/d/releases/latest",
            Answer::redirect("/x/releases/tag/latest"),
        ),
    ])
    .await;
    let client = |name: &str| {
        Client::new(
            Source::loopback(&format!("http://127.0.0.1:{}/{name}/releases", server.port)).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(client("a").latest().await.unwrap(), Some(v("0.4.2")));
    assert_eq!(client("b").latest().await.unwrap(), None);
    let err = client("c").latest().await.unwrap_err().to_string();
    assert!(err.contains("HTTP 500"), "{err}");
    let err = format!("{:#}", client("d").latest().await.unwrap_err());
    assert!(err.contains("not a version"), "{err}");
    // 404: nothing published either.
    assert_eq!(client("e").latest().await.unwrap(), None);
    // The redirect is read, not followed.
    assert!(!server.paths().iter().any(|path| path.contains("/tag/")));
}

#[tokio::test]
async fn check_says_whether_a_newer_release_is_out() {
    let server = start(release("0.3.0", b"new", None)).await;
    let installed = Installed::new();
    let options = Options {
        check: true,
        version: None,
    };
    let (out, _) = update(&server, &options, &installed.place("0.2.0")).await;
    assert_eq!(
        out.unwrap(),
        "junees 0.3.0 is out (this is 0.2.0): run `junees update`"
    );
    assert_eq!(server.paths(), ["/releases/latest"]);
    assert_eq!(installed.contents(), "old binary");
}

#[tokio::test]
async fn replaces_the_binary_with_a_newer_release() {
    let server = start(release("0.3.0", b"new binary", None)).await;
    let installed = Installed::new();
    let (out, said) = update(&server, &Options::default(), &installed.place("0.2.0")).await;
    let out = out.unwrap();
    assert_eq!(
        out,
        format!(
            "junees 0.2.0 -> 0.3.0: replaced {}",
            installed.real().display()
        )
    );
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].starts_with(&format!("downloading junees-0.3.0-{TARGET}.tar.gz")));
    // The file the link points at was replaced; the link stays a link.
    assert_eq!(installed.contents(), "new binary");
    let link = installed.dir.path().join("link/junees");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let mode = std::fs::metadata(installed.real())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755);
    assert_eq!(installed.bin_files(), ["junees"]);
    assert_eq!(
        server.paths(),
        [
            "/releases/latest".to_string(),
            "/releases/download/v0.3.0/SHA256SUMS".to_string(),
            format!("/releases/download/v0.3.0/junees-0.3.0-{TARGET}.tar.gz"),
        ]
    );
}

#[tokio::test]
async fn leaves_an_up_to_date_binary_alone() {
    let server = start(release("0.3.0", b"new binary", None)).await;
    let installed = Installed::new();
    for current in ["0.3.0", "0.4.0"] {
        let (out, _) = update(&server, &Options::default(), &installed.place(current)).await;
        let out = out.unwrap();
        assert!(
            out.contains("is the latest release") || out.contains("is newer than"),
            "{out}"
        );
    }
    let options = Options {
        check: false,
        version: Some(v("v0.2.0")),
    };
    let (out, _) = update(&server, &options, &installed.place("0.2.0")).await;
    assert_eq!(out.unwrap(), "junees 0.2.0 is already installed");
    assert!(server.paths().iter().all(|path| path == "/releases/latest"));
    assert_eq!(installed.contents(), "old binary");
}

#[tokio::test]
async fn installs_the_version_asked_for() {
    let server = start(release("0.1.0", b"older binary", None)).await;
    let installed = Installed::new();
    let options = Options {
        check: false,
        version: Some(v("0.1.0")),
    };
    let (out, said) = update(&server, &options, &installed.place("0.2.0")).await;
    assert!(out.unwrap().starts_with("junees 0.2.0 -> 0.1.0: replaced"));
    assert!(said[0].contains("older than this junees"), "{said:?}");
    assert_eq!(installed.contents(), "older binary");
    // `latest` was not asked.
    assert!(!server.paths().iter().any(|path| path.ends_with("/latest")));
}

#[tokio::test]
async fn refuses_an_archive_that_does_not_match_sha256sums() {
    let wrong = "1".repeat(64);
    let server = start(release("0.3.0", b"new binary", Some(&wrong))).await;
    let installed = Installed::new();
    let (out, _) = update(&server, &Options::default(), &installed.place("0.2.0")).await;
    let err = out.unwrap_err().to_string();
    assert!(err.contains("does not match SHA256SUMS"), "{err}");
    assert!(err.contains("nothing was replaced"), "{err}");
    assert_eq!(installed.contents(), "old binary");
    assert_eq!(installed.bin_files(), ["junees"]);
}

#[tokio::test]
async fn refuses_a_release_without_this_target_in_sha256sums() {
    let mut answers = release("0.3.0", b"new binary", None);
    answers[1].1 = Answer::ok(format!("{}  junees-0.3.0-other.tar.gz\n", "0".repeat(64)));
    let server = start(answers).await;
    let installed = Installed::new();
    let (out, _) = update(&server, &Options::default(), &installed.place("0.2.0")).await;
    let err = out.unwrap_err().to_string();
    assert!(
        err.contains(&format!("lists no junees-0.3.0-{TARGET}.tar.gz")),
        "{err}"
    );
    assert_eq!(installed.contents(), "old binary");
}

#[tokio::test]
async fn refuses_a_redirect_to_another_host() {
    let mut answers = release("0.3.0", b"new binary", None);
    let server = start(answers.clone()).await;
    // Same server, another host name: not where the release lives.
    answers[2].1 = Answer::redirect(&format!(
        "http://localhost:{}/releases/download/v0.3.0/x.tar.gz",
        server.port
    ));
    answers[1].1 = Answer::redirect("https://evil.example/SHA256SUMS");
    let other = start(answers).await;
    let installed = Installed::new();
    let (out, _) = update(&other, &Options::default(), &installed.place("0.2.0")).await;
    let err = format!("{:#}", out.unwrap_err());
    assert!(err.contains("cannot download SHA256SUMS"), "{err}");
    assert!(err.contains("redirected to evil.example"), "{err}");
    // And for the archive, once SHA256SUMS came.
    let mut answers = release("0.3.0", b"new binary", None);
    answers[2].1 = Answer::redirect(&format!(
        "http://localhost:{}/releases/download/v0.3.0/x.tar.gz",
        server.port
    ));
    let third = start(answers).await;
    let (out, _) = update(&third, &Options::default(), &installed.place("0.2.0")).await;
    let err = format!("{:#}", out.unwrap_err());
    assert!(err.contains("redirected to localhost"), "{err}");
    assert_eq!(installed.contents(), "old binary");
    // The redirect target was never asked.
    assert!(
        server
            .paths()
            .iter()
            .all(|path| !path.ends_with("x.tar.gz"))
    );
}

#[tokio::test]
async fn follows_a_redirect_on_the_same_server() {
    let mut answers = release("0.3.0", b"new binary", None);
    let tar = answers[2].1.clone();
    answers.push(("/files/archive".to_string(), tar));
    answers[2].1 = Answer::redirect("/files/archive");
    let server = start(answers).await;
    let installed = Installed::new();
    let (out, _) = update(&server, &Options::default(), &installed.place("0.2.0")).await;
    out.unwrap();
    assert_eq!(installed.contents(), "new binary");
}

#[tokio::test]
async fn caps_the_download_size() {
    let mut big = Answer::ok(vec![b'x'; 5000]);
    let server = Server::start(vec![
        ("/releases/download/v0.3.0/sized", big.clone()),
        ("/releases/download/v0.3.0/unsized", {
            big.no_length = true;
            big
        }),
    ])
    .await;
    let client = server.client();
    for name in ["sized", "unsized"] {
        let err = client
            .download(&v("0.3.0"), name, 4096)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("4096"), "{name}: {err}");
        assert_eq!(
            client
                .download(&v("0.3.0"), name, 5000)
                .await
                .unwrap()
                .len(),
            5000
        );
    }
    let err = client
        .download(&v("0.3.0"), "missing", 10)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP 404"), "{err}");
}

#[test]
fn takes_only_a_regular_binary_out_of_the_archive() {
    let tar = archive("0.3.0", b"bin");
    let stem = install::stem(&v("0.3.0"), TARGET);
    assert_eq!(
        install::extract(&tar, &format!("{stem}/junees")).unwrap(),
        b"bin"
    );
    let err = install::extract(&tar, "other/junees")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot unpack other/junees"), "{err}");
    let err = install::extract(b"not a tar", &format!("{stem}/junees"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot unpack"), "{err}");
    // A link in place of the binary.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(&stem)).unwrap();
    std::os::unix::fs::symlink("/bin/sh", dir.path().join(&stem).join("junees")).unwrap();
    let out = dir.path().join("link.tar.gz");
    assert!(
        std::process::Command::new("tar")
            .arg("-czf")
            .arg(&out)
            .arg("-C")
            .arg(dir.path())
            .arg(&stem)
            .status()
            .unwrap()
            .success()
    );
    let err = install::extract(&std::fs::read(out).unwrap(), &format!("{stem}/junees"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a regular file"), "{err}");
}

#[test]
fn a_folder_it_cannot_write_is_explained() {
    // SAFETY: getuid only returns this process's user id.
    if unsafe { libc::getuid() } == 0 {
        return;
    }
    let installed = Installed::new();
    let bin = installed.dir.path().join("bin");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = install::replace(&installed.dir.path().join("link/junees"), b"new");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cannot write in"), "{err}");
    assert!(err.contains("JUNEES_INSTALL_DIR"), "{err}");
    assert_eq!(installed.contents(), "old binary");
}

#[test]
fn notes_a_cargo_install_and_running_services() {
    let installed = Installed::new();
    let real = installed.real();
    let bin = real.parent().unwrap();
    assert!(install::notes(&real, None, None).is_empty());
    let notes = install::notes(&real, Some(bin), None);
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("came from `cargo install`"), "{notes:?}");
    let home = installed.dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    assert!(install::notes(&real, None, Some(&home)).is_empty());
    let _gateway = crate::gateway::lock(&home).unwrap();
    let notes = install::notes(&real, None, Some(&home));
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("Discord gateway"), "{notes:?}");
    assert!(notes[0].contains("restart it"), "{notes:?}");
}

#[test]
fn the_place_of_this_program() {
    let lookup = |key: &str| match key {
        "HOME" => Some("/home/someone".to_string()),
        _ => None,
    };
    let place = Place::current(&lookup).unwrap();
    assert_eq!(place.exe, std::env::current_exe().unwrap());
    assert_eq!(place.current, this_version());
    assert_eq!(
        place.cargo_bin,
        Some(PathBuf::from("/home/someone/.cargo/bin"))
    );
    assert_eq!(place.home, Some(PathBuf::from("/home/someone/.junees")));
    let lookup = |key: &str| (key == "CARGO_HOME").then(|| "/opt/cargo".to_string());
    assert_eq!(
        Place::current(&lookup).unwrap().cargo_bin.as_deref(),
        Some(Path::new("/opt/cargo/bin"))
    );
}

// install.sh, run by `sh` through the test sandbox of the bash tool, with
// a temporary HOME, install folder and TMPDIR, against the fake server.

/// Runs install.sh in `dir` with the variables `env` (shell words) and
/// `prefix` (commands to run first).
async fn install_sh(dir: &Path, prefix: &str, env: &str) -> crate::tools::ToolOutput {
    use crate::tools::{Bash, Tool};
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let command = format!(
        "{prefix}JUNEES_INSTALL_DIR={bin} TMPDIR={tmp} NO_PROXY=127.0.0.1 {env} sh {script}",
        bin = dir.join("bin").display(),
        tmp = tmp.display(),
    );
    Bash::for_test(dir)
        .run(serde_json::json!({ "command": command }))
        .await
}

/// The release build install.sh picks here, or `None` (test skipped).
fn script_target() -> Option<&'static str> {
    install::this_target()
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).unwrap().next().is_none()
}

#[tokio::test]
async fn install_sh_installs_the_latest_release() {
    let Some(target) = script_target() else {
        return;
    };
    let server = start(release_for("0.3.0", target, b"new binary", None)).await;
    let dir = tempfile::tempdir().unwrap();
    let output = install_sh(
        dir.path(),
        "",
        &format!("JUNEES_DOWNLOAD_BASE={}", server.base),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    let bin = dir.path().join("bin/junees");
    assert_eq!(std::fs::read(&bin).unwrap(), b"new binary");
    assert_eq!(
        std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(
        output
            .content
            .contains(&format!("installed junees 0.3.0 ({target})")),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("is not on your PATH"),
        "{}",
        output.content
    );
    assert_eq!(
        server.paths(),
        [
            "/releases/latest/download/SHA256SUMS".to_string(),
            format!("/releases/download/v0.3.0/junees-0.3.0-{target}.tar.gz"),
        ]
    );
    assert!(is_empty_dir(&dir.path().join("tmp")));
    let names: Vec<_> = std::fs::read_dir(dir.path().join("bin")).unwrap().collect();
    assert_eq!(names.len(), 1);
}

#[tokio::test]
async fn install_sh_installs_the_version_asked_for_over_an_old_one() {
    let Some(target) = script_target() else {
        return;
    };
    let server = start(release_for("0.1.0", target, b"v0.1.0 binary", None)).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("bin")).unwrap();
    std::fs::write(dir.path().join("bin/junees"), "old").unwrap();
    let output = install_sh(
        dir.path(),
        "",
        &format!("JUNEES_DOWNLOAD_BASE={} JUNEES_VERSION=v0.1.0", server.base),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(
        std::fs::read(dir.path().join("bin/junees")).unwrap(),
        b"v0.1.0 binary"
    );
    assert!(!server.paths().iter().any(|path| path.contains("latest")));
}

#[tokio::test]
async fn install_sh_refuses_an_archive_that_does_not_match() {
    let Some(target) = script_target() else {
        return;
    };
    let wrong = "2".repeat(64);
    let server = start(release_for("0.3.0", target, b"new binary", Some(&wrong))).await;
    let dir = tempfile::tempdir().unwrap();
    let output = install_sh(
        dir.path(),
        "",
        &format!("JUNEES_DOWNLOAD_BASE={}", server.base),
    )
    .await;
    assert!(output.is_error, "{}", output.content);
    assert!(
        output.content.contains("does not match SHA256SUMS"),
        "{}",
        output.content
    );
    assert!(!dir.path().join("bin/junees").exists());
    assert!(is_empty_dir(&dir.path().join("tmp")));
}

#[tokio::test]
async fn install_sh_refuses_an_unknown_architecture() {
    let server = start(release("0.3.0", b"new binary", None)).await;
    let dir = tempfile::tempdir().unwrap();
    // A `uname` that says riscv64, written by the shell itself.
    let fake = dir.path().join("fake");
    let prefix = format!(
        "mkdir -p {fake} && printf '#!/bin/sh\\ncase \"$1\" in -s) echo Linux;; *) echo riscv64;; esac\\n' > {fake}/uname && chmod +x {fake}/uname && PATH={fake}:$PATH ",
        fake = fake.display()
    );
    let output = install_sh(
        dir.path(),
        &prefix,
        &format!("JUNEES_DOWNLOAD_BASE={}", server.base),
    )
    .await;
    assert!(output.is_error, "{}", output.content);
    assert!(
        output
            .content
            .contains("there is no junees release for Linux on riscv64"),
        "{}",
        output.content
    );
    assert!(server.paths().is_empty());
    assert!(!dir.path().join("bin").exists());
}

#[tokio::test]
async fn install_sh_takes_only_a_test_server_on_this_computer() {
    let dir = tempfile::tempdir().unwrap();
    let output = install_sh(
        dir.path(),
        "",
        "JUNEES_DOWNLOAD_BASE=https://example.com/releases",
    )
    .await;
    assert!(output.is_error, "{}", output.content);
    assert!(
        output.content.contains("must be a server on this computer"),
        "{}",
        output.content
    );
    assert!(!dir.path().join("bin").exists());
}
