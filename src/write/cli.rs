//! `junees write`: makes a draft from the command line, drawing each level
//! over the last on a terminal.

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Draft, Kind, Lang, LevelDone, Options, Stop};
use crate::cancel::Cancel;

pub const HELP: &str = "\
Usage: junees write [options] \"<request>\"
       <request on standard input> | junees write [options]

Writes a long text coarse to fine (docs/write.md): a plan of sections,
then every section's gist at once, then every section's text at once.
On a terminal each level is drawn over the last; otherwise only the
final text is written. The usage line goes to standard error.

Options:
  --levels 2|3|4   2: plan, then text. 4: adds a paragraph outline and
                   writes every paragraph at once (default 3)
  --parallel N     requests at once, 1-8 (default: write_parallel, 4)
  --polish         one more pass over each seam between sections
  --out FILE       write the text to FILE instead of standard output; a
                   cancel (Ctrl-C) leaves the finished levels in FILE.partial
  --lang ko|en     the text's language (default: Korean if the request
                   has Hangul in it, else English)
  -h, --help       show this help";

/// The arguments of `junees write`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub levels: u8,
    pub parallel: Option<usize>,
    pub polish: bool,
    pub out: Option<PathBuf>,
    pub lang: Option<Lang>,
    /// `None`: read from standard input.
    pub request: Option<String>,
}

/// The arguments after `write`; `None` for `--help`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Args>> {
    let mut parsed = Args {
        levels: super::DEFAULT_LEVELS,
        parallel: None,
        polish: false,
        out: None,
        lang: None,
        request: None,
    };
    let mut words: Vec<String> = Vec::new();
    let mut args = args.into_iter();
    let mut options_done = false;
    while let Some(arg) = args.next() {
        if options_done || !arg.starts_with('-') || arg == "-" {
            words.push(arg);
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => args
                    .next()
                    .with_context(|| format!("{name} needs a value (see junees write --help)")),
            }
        };
        match name.as_str() {
            "-h" | "--help" => return Ok(None),
            "--" => options_done = true,
            "--levels" => {
                parsed.levels =
                    super::parse_levels(&value("--levels")?).map_err(anyhow::Error::msg)?
            }
            "--parallel" => {
                parsed.parallel =
                    Some(super::parse_parallel(&value("--parallel")?).map_err(anyhow::Error::msg)?)
            }
            "--polish" if inline.is_none() => parsed.polish = true,
            "--out" => {
                let out = value("--out")?;
                if out.trim().is_empty() {
                    bail!("--out needs a file name");
                }
                parsed.out = Some(PathBuf::from(out));
            }
            "--lang" => {
                parsed.lang = Some(Lang::parse(&value("--lang")?).map_err(anyhow::Error::msg)?)
            }
            _ => bail!("unknown option `{arg}` (see junees write --help)"),
        }
    }
    if !words.is_empty() {
        parsed.request = Some(words.join(" "));
    }
    Ok(Some(parsed))
}

/// Bytes read from standard input at most: past [`super::MAX_REQUEST_CHARS`]
/// characters of any script.
const STDIN_BYTES: u64 = (super::MAX_REQUEST_CHARS as u64) * 4 + 1;

fn read_request(args: &Args) -> Result<String> {
    if let Some(request) = &args.request {
        return Ok(request.clone());
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        bail!("give the request as an argument or on standard input (see junees write --help)");
    }
    let mut bytes = Vec::new();
    stdin
        .lock()
        .take(STDIN_BYTES)
        .read_to_end(&mut bytes)
        .context("cannot read the request from standard input")?;
    String::from_utf8(bytes).context("the request on standard input is not UTF-8")
}

/// Runs `junees write` with the settings `get` reads. The exit code: 0
/// when the draft is done (failed sections are listed on standard
/// error), 130 after a cancel.
pub async fn main(args: Args, get: impl Fn(&str) -> Option<String>) -> Result<u8> {
    let config = crate::config::Config::from_lookup(&get)?;
    let request = read_request(&args)?;
    super::check_request(&request).map_err(anyhow::Error::msg)?;
    let prices = crate::usage::Prices::from_setting(get(crate::usage::PRICES_ENV).as_deref())
        .unwrap_or_else(|err| {
            eprintln!("  !! {err}; using the built-in prices");
            crate::usage::Prices::default()
        });
    let provider = crate::provider::from_config(&config)?;
    let options = Options {
        levels: args.levels,
        parallel: args.parallel.unwrap_or(config.write_parallel),
        polish: args.polish,
        lang: args.lang,
        max_tokens: config.max_tokens,
    };
    let cancel = Cancel::new();
    let on_signal = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_signal.cancel();
        }
    });
    let mut screen = std::io::stdout().is_terminal().then(Screen::enter);
    let (sender, mut levels) = tokio::sync::mpsc::unbounded_channel();
    let work = super::run(&*provider, &request, &options, &cancel, Some(&sender));
    tokio::pin!(work);
    let mut view = View::default();
    let draft = loop {
        tokio::select! {
            draft = &mut work => break draft,
            Some(level) = levels.recv() => {
                view.update(&level);
                if let Some(screen) = &mut screen {
                    screen.draw(&view.render(&level, options.levels));
                } else {
                    eprintln!("  [draft] {}", super::level_line(&level));
                }
            }
        }
    };
    signal.abort();
    // Back to the normal screen before the final text.
    drop(screen);
    finish(&draft, args.out.as_deref(), &prices)
}

/// Writes the result where it goes and the usage to standard error.
fn finish(draft: &Draft, out: Option<&Path>, prices: &crate::usage::Prices) -> Result<u8> {
    let text = draft.text();
    let code = match &draft.stop {
        Stop::Failed(reason) => {
            eprintln!("{}", draft.summary(Some(prices)));
            bail!("draft failed: {reason}");
        }
        Stop::Done => {
            match out {
                Some(path) => {
                    std::fs::write(path, &text)
                        .with_context(|| format!("cannot write {}", path.display()))?;
                    eprintln!("저장했습니다: {}", path.display());
                }
                None => print!("{text}"),
            }
            0
        }
        Stop::Cancelled => {
            eprintln!(
                "취소했습니다. {}단계까지 끝난 글을 남깁니다.",
                draft.levels_done
            );
            if draft.levels_done > 0 {
                match out {
                    Some(path) => {
                        let partial = partial_path(path);
                        std::fs::write(&partial, &text)
                            .with_context(|| format!("cannot write {}", partial.display()))?;
                        eprintln!("저장했습니다: {}", partial.display());
                    }
                    None => print!("{text}"),
                }
            }
            130
        }
    };
    let _ = std::io::stdout().flush();
    let failed = draft.failed_sections();
    if !failed.is_empty() {
        eprintln!(
            "  !! 생성에 실패해 앞 단계의 글로 대신한 절: {}",
            failed.join(", ")
        );
    }
    eprintln!("{}", draft.summary(Some(prices)));
    for level in draft.by_level() {
        eprintln!(
            "  {}단계 {}: 호출 {}번{}, 입력 {} / 출력 {} 토큰",
            level.level,
            level.kind.label(),
            level.calls,
            if level.failed_calls > 0 {
                format!(" (실패 {})", level.failed_calls)
            } else {
                String::new()
            },
            crate::usage::report::thousands(
                level.usage.input_tokens
                    + level.usage.cache_creation_input_tokens
                    + level.usage.cache_read_input_tokens
            ),
            crate::usage::report::thousands(level.usage.output_tokens),
        );
    }
    Ok(code)
}

/// `FILE.partial` next to `FILE`.
pub fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".partial");
    PathBuf::from(name)
}

/// What the terminal shows: the title and each section's concept from
/// level 1, and the latest level's text.
#[derive(Debug, Default)]
pub struct View {
    title: String,
    concepts: HashMap<String, String>,
}

impl View {
    pub fn update(&mut self, level: &LevelDone) {
        self.title = level.title.clone();
        if level.kind == Kind::Concept {
            self.concepts = level.sections.iter().cloned().collect();
        }
    }

    /// The draft at `level`, under a line that says which level of how
    /// many it is.
    pub fn render(&self, level: &LevelDone, levels: u8) -> String {
        let mut out = format!(
            "[{}/{}단계 {}]\n\n# {}\n",
            level.level.min(levels),
            levels,
            level.kind.label(),
            self.title
        );
        for (id, text) in &level.sections {
            if level.kind == Kind::Concept {
                out.push_str(&format!("\n- {text}"));
            } else {
                let concept = self.concepts.get(id).map_or(id.as_str(), String::as_str);
                out.push_str(&format!("\n## {concept}\n\n{text}\n"));
            }
        }
        out.push('\n');
        out
    }
}

/// The terminal's alternate screen while the levels are drawn: each level
/// replaces the last, and leaving it brings back what was on the terminal
/// before, then the final text is printed below.
struct Screen {
    stdout: std::io::Stdout,
}

impl Screen {
    fn enter() -> Self {
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b[?1049h");
        let _ = stdout.flush();
        Self { stdout }
    }

    fn draw(&mut self, text: &str) {
        // Home, clear; text with control characters shown, not run.
        let shown: String = text
            .split('\n')
            .map(crate::approval::visible)
            .collect::<Vec<_>>()
            .join("\r\n");
        let _ = write!(self.stdout, "\x1b[H\x1b[2J{shown}");
        let _ = self.stdout.flush();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = write!(self.stdout, "\x1b[?1049l");
        let _ = self.stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Option<Args>> {
        parse(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn options_and_the_request_are_read() {
        let parsed = args(&[
            "--levels",
            "4",
            "--parallel=8",
            "--polish",
            "--out",
            "a.md",
            "--lang",
            "en",
            "write",
            "a",
            "README",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed,
            Args {
                levels: 4,
                parallel: Some(8),
                polish: true,
                out: Some(PathBuf::from("a.md")),
                lang: Some(Lang::En),
                request: Some("write a README".to_string()),
            }
        );
        let parsed = args(&["--", "--levels"]).unwrap().unwrap();
        assert_eq!(parsed.request.as_deref(), Some("--levels"));
        assert_eq!(args(&[]).unwrap().unwrap().request, None);
        assert_eq!(args(&["x", "--help"]).unwrap(), None);
        for bad in [
            &["--levels", "5"][..],
            &["--parallel", "9"],
            &["--parallel"],
            &["--lang", "fr"],
            &["--out", ""],
            &["--polish=yes"],
            &["--what"],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_level_is_drawn_under_the_concepts() {
        let mut view = View::default();
        let concepts = LevelDone {
            level: 1,
            kind: Kind::Concept,
            title: "T".to_string(),
            sections: vec![
                ("s1".into(), "first".into()),
                ("s2".into(), "second".into()),
            ],
        };
        view.update(&concepts);
        assert_eq!(
            view.render(&concepts, 3),
            "[1/3단계 개념]\n\n# T\n\n- first\n- second\n"
        );
        let gists = LevelDone {
            level: 2,
            kind: Kind::Gist,
            title: "T".to_string(),
            sections: vec![("s1".into(), "g1".into()), ("s2".into(), "g2".into())],
        };
        view.update(&gists);
        assert_eq!(
            view.render(&gists, 3),
            "[2/3단계 요지]\n\n# T\n\n## first\n\ng1\n\n## second\n\ng2\n\n"
        );
        assert_eq!(
            partial_path(Path::new("out/a.md")),
            PathBuf::from("out/a.md.partial")
        );
    }
}
