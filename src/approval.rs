//! Asking the person at the terminal before risky tool calls run
//! (`JUNEES_APPROVAL`). Only the conversation mode asks: `-p` and task mode
//! have no one to ask and run tool calls under the command policy alone.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader, Lines};

use crate::config::Config;
use crate::tools::{CommandPolicy, OUTSIDE, ToolOutput, ToolSettings, Verdict, Workspace};

/// Which tool calls need the person's approval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApprovalMode {
    /// None.
    Off,
    /// bash commands with a caution (see [`CommandPolicy::cautions`]), and
    /// `write_file`/`edit_file` outside the working folder.
    #[default]
    Risky,
    /// Every bash, `write_file` and `edit_file` call.
    All,
}

impl ApprovalMode {
    /// Reads `JUNEES_APPROVAL`: `off`, `risky` (the default) or `all`.
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::to_ascii_lowercase).as_deref() {
            None | Some("risky") => Ok(Self::Risky),
            Some("off") => Ok(Self::Off),
            Some("all") => Ok(Self::All),
            Some(_) => bail!(
                "JUNEES_APPROVAL must be off, risky or all, got `{}`",
                value.unwrap_or_default()
            ),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Risky => "risky",
            Self::All => "all",
        }
    }
}

/// Decides whether a tool call may run. The agent asks it before every
/// call (see [`crate::agent::Agent::with_gate`]).
#[async_trait]
pub trait ToolGate: Send {
    /// `None` lets the call run; `Some(output)` is the call's result
    /// instead, and the tool does not run.
    async fn check(&mut self, tool: &str, input: &Value) -> Option<ToolOutput>;
}

/// A tool call that needs the person's approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub tool: String,
    /// The command, or the path a file tool writes.
    pub target: String,
    /// Why it needs approval. Empty for calls only `all` asks about.
    pub reasons: Vec<String>,
    /// What `edit_file` changes or `write_file` writes, shortened.
    pub preview: Option<String>,
    /// What an "always" answer allows for the rest of the session, as
    /// `tool: kind`.
    pub kinds: Vec<String>,
}

/// The person's answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    /// Yes, and the same kinds from now on without asking.
    Always,
    /// No, with a reason the model gets to read.
    NoBecause(String),
}

/// Asks the person about a call. Tests script the answers.
#[async_trait]
pub trait Ask: Send {
    async fn ask(&mut self, request: &Request) -> Answer;
}

/// The result of a declined call; the model reads it and changes its plan.
pub fn declined(reason: &str) -> ToolOutput {
    ToolOutput::error(format!("the user declined: {reason}"))
}

/// Lines shown of an `edit_file` side or a `write_file` content.
const PREVIEW_LINES: usize = 8;
/// Characters shown of one preview line.
const PREVIEW_LINE_CHARS: usize = 160;

/// The [`ToolGate`] of the conversation mode.
pub struct Approval {
    mode: ApprovalMode,
    /// The same rules as bash, so commands it refuses are not asked about.
    policy: CommandPolicy,
    workspace: Workspace,
    /// `None` when there is no one to ask: calls that need approval are
    /// declined.
    asker: Option<Box<dyn Ask>>,
    /// Kinds the person allowed for the rest of the session.
    always: HashSet<String>,
}

impl Approval {
    /// A gate with no one to ask yet; see [`Approval::asking`].
    pub fn new(mode: ApprovalMode, settings: &ToolSettings, workspace: Workspace) -> Self {
        Self {
            mode,
            policy: CommandPolicy::with_settings(settings),
            workspace,
            asker: None,
            always: HashSet::new(),
        }
    }

    pub fn asking(mut self, asker: Box<dyn Ask>) -> Self {
        self.asker = Some(asker);
        self
    }

    /// The question for a call, or `None` when it may run without one.
    /// Calls with input the tool will reject anyway, and bash commands the
    /// policy refuses, run (and fail) without asking.
    pub fn request(&self, tool: &str, input: &Value) -> Option<Request> {
        if self.mode == ApprovalMode::Off {
            return None;
        }
        let (target, reasons, kinds, preview) = match tool {
            "bash" => {
                let command = input.get("command")?.as_str()?;
                if let Verdict::Deny(_) = self.policy.check(command) {
                    return None;
                }
                let cautions = self.policy.cautions(command, &self.workspace);
                let mut kinds: Vec<String> = Vec::new();
                for caution in &cautions {
                    let kind = format!("bash: {}", caution.kind);
                    if !kinds.contains(&kind) {
                        kinds.push(kind);
                    }
                }
                let reasons = cautions.into_iter().map(|caution| caution.reason).collect();
                (command, reasons, kinds, None)
            }
            "write_file" | "edit_file" => {
                let path = input.get("path")?.as_str()?;
                let preview = if tool == "edit_file" {
                    edit_preview(input)
                } else {
                    write_preview(input, &self.workspace.root().join(path))
                };
                if self.workspace.contains(path) {
                    (path, Vec::new(), Vec::new(), preview)
                } else {
                    (
                        path,
                        vec![format!("writes to `{path}` outside the working folder")],
                        vec![format!("{tool}: {OUTSIDE}")],
                        preview,
                    )
                }
            }
            _ => return None,
        };
        let kinds = match (kinds.is_empty(), self.mode) {
            (false, _) => kinds,
            (true, ApprovalMode::All) => vec![format!("{tool}: calls without a caution")],
            (true, _) => return None,
        };
        Some(Request {
            tool: tool.to_string(),
            target: target.to_string(),
            reasons,
            preview,
            kinds,
        })
    }
}

#[async_trait]
impl ToolGate for Approval {
    async fn check(&mut self, tool: &str, input: &Value) -> Option<ToolOutput> {
        let request = self.request(tool, input)?;
        if request.kinds.iter().all(|kind| self.always.contains(kind)) {
            return None;
        }
        let Some(asker) = &mut self.asker else {
            return Some(ToolOutput::error(format!(
                "not run: this call needs the user's approval (JUNEES_APPROVAL={}), but junees \
                 cannot ask because its input is not a terminal",
                self.mode.name()
            )));
        };
        match asker.ask(&request).await {
            Answer::Yes => None,
            Answer::Always => {
                self.always.extend(request.kinds);
                None
            }
            Answer::No => Some(declined("no reason given")),
            Answer::NoBecause(reason) => Some(declined(&reason)),
        }
    }
}

/// The gate of the conversation mode, or `None` with `JUNEES_APPROVAL=off`.
/// It asks on the terminal through `input`, which the prompt reads too;
/// when stdin is not a terminal it declines every call that needs approval
/// and says so on stderr now.
pub fn for_conversation(config: &Config, cwd: &Path, input: &SharedLines) -> Option<Approval> {
    if config.approval == ApprovalMode::Off {
        return None;
    }
    let approval = Approval::new(
        config.approval,
        &config.tools,
        Workspace::new(cwd).with_temp_dirs(),
    );
    if std::io::stdin().is_terminal() {
        return Some(approval.asking(Box::new(TerminalAsk::new(
            input.clone(),
            Box::new(std::io::stderr()),
        ))));
    }
    eprintln!(
        "  !! stdin is not a terminal: tool calls that need approval (JUNEES_APPROVAL={}) are \
         declined; set JUNEES_APPROVAL=off to run them without asking",
        config.approval.name()
    );
    Some(approval)
}

fn edit_preview(input: &Value) -> Option<String> {
    let old = input.get("old_string")?.as_str()?;
    let new = input.get("new_string")?.as_str()?;
    let mut preview = String::new();
    if input.get("replace_all").and_then(Value::as_bool) == Some(true) {
        preview.push_str("(every occurrence)\n");
    }
    preview_lines(&mut preview, "- ", old);
    preview_lines(&mut preview, "+ ", new);
    Some(preview)
}

fn write_preview(input: &Value, path: &Path) -> Option<String> {
    let content = input.get("content")?.as_str()?;
    let what = if path.exists() {
        "replaces the file"
    } else {
        "new file"
    };
    let mut preview = format!("({what}, {} lines)\n", content.lines().count());
    preview_lines(&mut preview, "+ ", content);
    Some(preview)
}

/// Appends the first lines of `text`, each after `marker` and shortened on
/// a character boundary.
fn preview_lines(preview: &mut String, marker: &str, text: &str) {
    let total = text.lines().count();
    for line in text.lines().take(PREVIEW_LINES) {
        preview.push_str(marker);
        if line.chars().count() > PREVIEW_LINE_CHARS {
            preview.extend(line.chars().take(PREVIEW_LINE_CHARS));
            preview.push_str("...");
        } else {
            preview.push_str(line);
        }
        preview.push('\n');
    }
    if total > PREVIEW_LINES {
        preview.push_str(&format!(
            "{marker}... {} more lines\n",
            total - PREVIEW_LINES
        ));
    }
}

/// The question as shown on the terminal, without the choices. What the
/// model wrote goes through [`visible`], so it cannot move the cursor or
/// erase what the question shows.
pub fn render(request: &Request) -> String {
    let mut lines = request.target.lines();
    let mut text = format!(
        "  ?? approve {}: {}\n",
        visible(&request.tool),
        visible(lines.next().unwrap_or_default())
    );
    for line in lines {
        text.push_str(&format!("     | {}\n", visible(line)));
    }
    for reason in &request.reasons {
        for line in reason.lines() {
            text.push_str(&format!("     * {}\n", visible(line)));
        }
    }
    if let Some(preview) = &request.preview {
        for line in preview.lines() {
            text.push_str(&format!("     {}\n", visible(line)));
        }
    }
    text
}

/// One line of `text` with control characters (other than tab) and
/// bidirectional formatting characters written out as escapes (`\r`,
/// `\u{1b}`, `\u{202e}`), so they show instead of acting on the terminal.
fn visible(line: &str) -> String {
    let mut shown = String::with_capacity(line.len());
    for c in line.chars() {
        let hidden = (c.is_control() && c != '\t')
            || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if hidden {
            shown.extend(c.escape_default());
        } else {
            shown.push(c);
        }
    }
    shown
}

const CHOICES: &str = "  [y] run  [n] decline  [a] always allow this kind this session  \
                       [e] decline with a reason > ";

/// Asks on the terminal: the question goes to `output` (stderr), the
/// answer comes from the lines the prompt reads too.
pub struct TerminalAsk {
    input: SharedLines,
    output: Box<dyn Write + Send>,
}

impl TerminalAsk {
    pub fn new(input: SharedLines, output: Box<dyn Write + Send>) -> Self {
        Self { input, output }
    }

    fn say(&mut self, text: &str) {
        // Nothing to do about a broken terminal; the read below ends it.
        let _ = self.output.write_all(text.as_bytes());
        let _ = self.output.flush();
    }

    /// The next line, or `None` at the end of input or on a read error.
    async fn line(&mut self) -> Option<String> {
        self.input.next_line().await.ok().flatten()
    }
}

#[async_trait]
impl Ask for TerminalAsk {
    async fn ask(&mut self, request: &Request) -> Answer {
        self.say(&render(request));
        self.say(&format!(
            "     always allows: {}\n",
            request.kinds.join(", ")
        ));
        loop {
            self.say(CHOICES);
            let Some(line) = self.line().await else {
                self.say("\n");
                return Answer::No;
            };
            let line = line.trim();
            let (choice, rest) = line.split_once(' ').unwrap_or((line, ""));
            match choice.to_ascii_lowercase().as_str() {
                "y" | "yes" => return Answer::Yes,
                "n" | "no" => return Answer::No,
                "a" | "always" => return Answer::Always,
                "e" => {
                    let mut reason = rest.trim().to_string();
                    if reason.is_empty() {
                        self.say("  reason > ");
                        reason = self.line().await.unwrap_or_default().trim().to_string();
                    }
                    return if reason.is_empty() {
                        Answer::No
                    } else {
                        Answer::NoBecause(reason)
                    };
                }
                _ => self.say("  answer y, n, a or e\n"),
            }
        }
    }
}

type LineReader = Lines<BufReader<Box<dyn AsyncRead + Send + Unpin>>>;

/// Input lines shared by the prompt and the approval questions, so a line
/// typed after a cancelled question still reaches the prompt. Cheap to
/// clone.
#[derive(Clone)]
pub struct SharedLines(Arc<tokio::sync::Mutex<LineReader>>);

impl SharedLines {
    pub fn new(reader: impl AsyncRead + Send + Unpin + 'static) -> Self {
        let reader: Box<dyn AsyncRead + Send + Unpin> = Box::new(reader);
        Self(Arc::new(tokio::sync::Mutex::new(
            BufReader::new(reader).lines(),
        )))
    }

    pub fn stdin() -> Self {
        Self::new(tokio::io::stdin())
    }

    /// The next line, `None` at the end of input. Cancel-safe: dropping the
    /// future keeps a line in progress for the next call.
    pub async fn next_line(&self) -> std::io::Result<Option<String>> {
        self.0.lock().await.next_line().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        (dir, workspace)
    }

    fn approval(mode: ApprovalMode, workspace: &Workspace) -> Approval {
        Approval::new(mode, &ToolSettings::default(), workspace.clone())
    }

    fn bash(command: &str) -> Value {
        json!({ "command": command })
    }

    /// Answers from a list and keeps every question.
    #[derive(Clone, Default)]
    struct Scripted {
        answers: Arc<Mutex<VecDeque<Answer>>>,
        asked: Arc<Mutex<Vec<Request>>>,
    }

    impl Scripted {
        fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
            Self {
                answers: Arc::new(Mutex::new(answers.into_iter().collect())),
                ..Self::default()
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked
                .lock()
                .unwrap()
                .iter()
                .map(|request| request.target.clone())
                .collect()
        }
    }

    #[async_trait]
    impl Ask for Scripted {
        async fn ask(&mut self, request: &Request) -> Answer {
            self.asked.lock().unwrap().push(request.clone());
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("asked more often than scripted")
        }
    }

    #[test]
    fn parses_the_mode() {
        assert_eq!(ApprovalMode::parse(None).unwrap(), ApprovalMode::Risky);
        assert_eq!(ApprovalMode::parse(Some("off")).unwrap(), ApprovalMode::Off);
        assert_eq!(
            ApprovalMode::parse(Some("RISKY")).unwrap(),
            ApprovalMode::Risky
        );
        assert_eq!(ApprovalMode::parse(Some("all")).unwrap(), ApprovalMode::All);
        let err = ApprovalMode::parse(Some("yes")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "JUNEES_APPROVAL must be off, risky or all, got `yes`"
        );
    }

    #[test]
    fn risky_asks_only_about_cautioned_commands_and_outside_writes() {
        let (dir, workspace) = workspace();
        let gate = approval(ApprovalMode::Risky, &workspace);
        assert_eq!(gate.request("bash", &bash("ls -la && cargo test")), None);
        let request = gate
            .request("bash", &bash("rm -rf build && git push"))
            .unwrap();
        assert_eq!(
            request,
            Request {
                tool: "bash".into(),
                target: "rm -rf build && git push".into(),
                reasons: vec![
                    "`rm` deletes files".into(),
                    "`git push` publishes commits".into()
                ],
                preview: None,
                kinds: vec!["bash: delete files".into(), "bash: git push".into()],
            }
        );
        // The policy refuses these itself, so no one is asked.
        assert_eq!(gate.request("bash", &bash("rm -rf / && git push")), None);
        assert_eq!(gate.request("bash", &bash("sudo rm a")), None);
        // Input the tool rejects anyway, and other tools.
        assert_eq!(gate.request("bash", &json!({"cmd": "rm a"})), None);
        assert_eq!(
            gate.request("read_file", &json!({"path": "/etc/hosts"})),
            None
        );
        assert_eq!(
            gate.request("mark_feature_passing", &json!({"id": 1})),
            None
        );

        let write = json!({"path": "notes.md", "content": "hi"});
        assert_eq!(gate.request("write_file", &write), None);
        let outside = dir.path().parent().unwrap().join("x.md");
        let outside = outside.to_str().unwrap();
        let request = gate
            .request("write_file", &json!({"path": outside, "content": "a\nb"}))
            .unwrap();
        assert_eq!(
            request.reasons,
            [format!("writes to `{outside}` outside the working folder")]
        );
        assert_eq!(
            request.kinds,
            ["write_file: write outside the working folder"]
        );
        assert_eq!(
            request.preview.as_deref(),
            Some("(new file, 2 lines)\n+ a\n+ b\n")
        );
        let edit = json!({"path": "../x.rs", "old_string": "a", "new_string": "b"});
        assert_eq!(
            gate.request("edit_file", &edit).unwrap().kinds,
            ["edit_file: write outside the working folder"]
        );
    }

    #[test]
    fn a_dotdot_after_a_link_is_judged_where_the_write_lands() {
        let (dir, workspace) = workspace();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(elsewhere.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("sub"), dir.path().join("out")).unwrap();
        let gate = approval(ApprovalMode::Risky, &workspace);
        // `out/..` is `elsewhere`, not the working folder.
        let write = json!({"path": "out/../escaped.txt", "content": "x"});
        assert_eq!(
            gate.request("write_file", &write).unwrap().kinds,
            ["write_file: write outside the working folder"]
        );
        assert_eq!(
            gate.request("bash", &bash("echo x > out/../escaped.txt"))
                .unwrap()
                .kinds,
            ["bash: write outside the working folder"]
        );
    }

    #[test]
    fn all_asks_about_every_bash_and_file_write_and_off_about_none() {
        let (_dir, workspace) = workspace();
        let all = approval(ApprovalMode::All, &workspace);
        let request = all.request("bash", &bash("ls")).unwrap();
        assert_eq!(request.reasons, Vec::<String>::new());
        assert_eq!(request.kinds, ["bash: calls without a caution"]);
        let edit = json!({"path": "a.rs", "old_string": "x", "new_string": "y"});
        assert_eq!(
            all.request("edit_file", &edit).unwrap().kinds,
            ["edit_file: calls without a caution"]
        );
        assert_eq!(all.request("read_file", &json!({"path": "a.rs"})), None);
        // Cautions keep their own kinds.
        assert_eq!(
            all.request("bash", &bash("git push")).unwrap().kinds,
            ["bash: git push"]
        );

        let off = approval(ApprovalMode::Off, &workspace);
        assert_eq!(off.request("bash", &bash("rm -rf build")), None);
        assert_eq!(
            off.request("write_file", &json!({"path": "/etc/x", "content": ""})),
            None
        );
    }

    #[test]
    fn previews_show_a_short_diff() {
        let (dir, workspace) = workspace();
        let gate = approval(ApprovalMode::All, &workspace);
        let long = "가".repeat(PREVIEW_LINE_CHARS + 5);
        let old: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        let edit = json!({
            "path": "a.rs", "old_string": old, "new_string": long, "replace_all": true
        });
        let preview = gate.request("edit_file", &edit).unwrap().preview.unwrap();
        let expected_new = format!("+ {}...\n", "가".repeat(PREVIEW_LINE_CHARS));
        let expected_old: String = (1..=8).map(|n| format!("- line {n}\n")).collect();
        assert_eq!(
            preview,
            format!("(every occurrence)\n{expected_old}- ... 2 more lines\n{expected_new}")
        );
        std::fs::write(dir.path().join("old.txt"), "x").unwrap();
        let write = json!({"path": "old.txt", "content": "new"});
        assert_eq!(
            gate.request("write_file", &write)
                .unwrap()
                .preview
                .as_deref(),
            Some("(replaces the file, 1 lines)\n+ new\n")
        );
    }

    #[tokio::test]
    async fn answers_run_or_decline_the_call() {
        let (_dir, workspace) = workspace();
        let asker = Scripted::new([
            Answer::Yes,
            Answer::No,
            Answer::NoBecause("use cargo clean instead".into()),
        ]);
        let mut gate = approval(ApprovalMode::Risky, &workspace).asking(Box::new(asker.clone()));
        let rm = bash("rm -rf target");
        assert_eq!(gate.check("bash", &rm).await, None);
        assert_eq!(
            gate.check("bash", &rm).await,
            Some(ToolOutput::error("the user declined: no reason given"))
        );
        assert_eq!(
            gate.check("bash", &rm).await,
            Some(ToolOutput::error(
                "the user declined: use cargo clean instead"
            ))
        );
        // Calls without a caution run without a question.
        assert_eq!(gate.check("bash", &bash("ls")).await, None);
        assert_eq!(asker.asked().len(), 3);
    }

    #[tokio::test]
    async fn always_allows_the_same_kinds_for_the_session() {
        let (_dir, workspace) = workspace();
        let asker = Scripted::new([Answer::Always, Answer::No, Answer::Yes]);
        let mut gate = approval(ApprovalMode::Risky, &workspace).asking(Box::new(asker.clone()));
        assert_eq!(gate.check("bash", &bash("rm a")).await, None);
        // Same kind: no question.
        assert_eq!(gate.check("bash", &bash("rm -rf b c")).await, None);
        assert_eq!(gate.check("bash", &bash("find . -delete")).await, None);
        // Another kind, or the allowed one with another: asked again.
        assert!(gate.check("bash", &bash("git push")).await.is_some());
        assert_eq!(gate.check("bash", &bash("rm d && git push")).await, None);
        assert_eq!(asker.asked(), ["rm a", "git push", "rm d && git push"]);
    }

    #[tokio::test]
    async fn without_a_terminal_calls_that_need_approval_are_declined() {
        let (_dir, workspace) = workspace();
        let mut gate = approval(ApprovalMode::Risky, &workspace);
        let output = gate.check("bash", &bash("rm a")).await.unwrap();
        assert!(output.is_error);
        assert_eq!(
            output.content,
            "not run: this call needs the user's approval (JUNEES_APPROVAL=risky), but junees \
             cannot ask because its input is not a terminal"
        );
        assert_eq!(gate.check("bash", &bash("ls")).await, None);
    }

    /// Collects what the terminal asker writes.
    #[derive(Clone, Default)]
    struct Screen(Arc<Mutex<Vec<u8>>>);

    impl Write for Screen {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Screen {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn the_question_shows_control_characters_instead_of_obeying_them() {
        let (_dir, workspace) = workspace();
        let gate = approval(ApprovalMode::Risky, &workspace);
        let command = "rm -rf ~/projects # \r\x1b[2Kecho tidy up build cache";
        let shown = render(&gate.request("bash", &bash(command)).unwrap());
        assert_eq!(
            shown.lines().next().unwrap(),
            "  ?? approve bash: rm -rf ~/projects # \\r\\u{1b}[2Kecho tidy up build cache"
        );
        let write = json!({"path": "/etc/x\u{202e}txt.sh", "content": "a\tb\x07\r\nc\u{8}"});
        let shown = render(&gate.request("write_file", &write).unwrap());
        assert_eq!(
            shown,
            "  ?? approve write_file: /etc/x\\u{202e}txt.sh\n     \
             * writes to `/etc/x\\u{202e}txt.sh` outside the working folder\n     \
             (new file, 2 lines)\n     + a\tb\\u{7}\n     + c\\u{8}\n"
        );
        assert!(
            !shown
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        );
    }

    async fn ask_with(input: &str) -> (Answer, String) {
        let request = Request {
            tool: "bash".into(),
            target: "rm a\nrm b".into(),
            reasons: vec!["`rm` deletes files".into()],
            preview: None,
            kinds: vec!["bash: delete files".into()],
        };
        let screen = Screen::default();
        let mut asker = TerminalAsk::new(
            SharedLines::new(std::io::Cursor::new(input.as_bytes().to_vec())),
            Box::new(screen.clone()),
        );
        let answer = asker.ask(&request).await;
        (answer, screen.text())
    }

    #[tokio::test]
    async fn the_terminal_asker_reads_answers() {
        let (answer, screen) = ask_with("y\n").await;
        assert_eq!(answer, Answer::Yes);
        assert_eq!(
            screen,
            format!(
                "  ?? approve bash: rm a\n     | rm b\n     * `rm` deletes files\n     \
                 always allows: bash: delete files\n{CHOICES}"
            )
        );
        assert_eq!(ask_with("No\n").await.0, Answer::No);
        assert_eq!(ask_with(" a \n").await.0, Answer::Always);
        let (answer, screen) = ask_with("maybe\nyes\n").await;
        assert_eq!(answer, Answer::Yes);
        assert!(screen.contains("answer y, n, a or e\n"), "{screen}");
        assert_eq!(
            ask_with("e\n지우지 마세요\n").await.0,
            Answer::NoBecause("지우지 마세요".into())
        );
        assert_eq!(
            ask_with("e keep the cache\n").await.0,
            Answer::NoBecause("keep the cache".into())
        );
        assert_eq!(ask_with("e\n\n").await.0, Answer::No);
        // End of input declines.
        assert_eq!(ask_with("").await.0, Answer::No);
    }

    #[tokio::test]
    async fn a_line_typed_after_a_cancelled_read_reaches_the_next_one() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let lines = SharedLines::new(reader);
        writer.write_all(b"hal").await.unwrap();
        let cancelled = tokio::time::timeout(Duration::from_millis(50), lines.next_line()).await;
        assert!(cancelled.is_err(), "the read should still wait");
        writer.write_all(b"lo\nnext\n").await.unwrap();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("hallo"));
        assert_eq!(
            lines.clone().next_line().await.unwrap().as_deref(),
            Some("next")
        );
    }
}
