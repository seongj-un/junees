//! The conversation mode on the `claude-code` backend: every turn is one
//! `claude -p` run in the current folder, and every turn after the first
//! continues the same Claude Code conversation with `--resume <id>`
//! (<https://code.claude.com/docs/en/headless>, "Continue conversations").
//!
//! Claude Code keeps the conversation and manages its context itself
//! (Junees does not clear or compact anything). The session journal keeps
//! a [`ClaudeTurn`] per turn: Claude Code's session id, so `-c` and
//! `/sessions` continue the same conversation, and a summary of the turn.

use std::path::PathBuf;

use anyhow::Result;

use super::{ClaudeCode, End, Outcome, Spawn, TOOLS, deny_rules, settings_rules};
use crate::agent::{Cancelled, Event};
use crate::cancel::Cancel;
use crate::message::Usage;
use crate::session::{ClaudeTurn, Entry, Journal, Transcript};
use crate::tools::ToolSettings;

/// A conversation run by Claude Code.
pub struct Chat {
    claude: ClaudeCode,
    /// Where `claude` runs: the folder Junees was started in.
    cwd: PathBuf,
    settings: ToolSettings,
    /// `--max-turns` of each turn.
    max_turns: usize,
    /// The turns so far and Claude Code's session id.
    transcript: Transcript,
    journal: Option<Box<dyn Journal>>,
    cancel: Cancel,
}

impl Chat {
    pub fn new(
        claude: ClaudeCode,
        cwd: PathBuf,
        settings: ToolSettings,
        max_turns: usize,
        cancel: Cancel,
    ) -> Self {
        Self {
            claude,
            cwd,
            settings,
            max_turns,
            transcript: Transcript::default(),
            journal: None,
            cancel,
        }
    }

    /// Continues the Claude Code conversation of a saved session and
    /// records from now on to `journal` (or nowhere).
    pub fn resume(&mut self, transcript: Transcript, journal: Option<Box<dyn Journal>>) {
        self.transcript = transcript;
        self.journal = journal;
    }

    /// Starts a new conversation, recorded to `journal` (or nowhere).
    pub fn reset(&mut self, journal: Option<Box<dyn Journal>>) {
        self.resume(Transcript::default(), journal);
    }

    pub fn claude(&self) -> &ClaudeCode {
        &self.claude
    }

    /// The model the next turns ask Claude Code for (`--model`); `None`
    /// leaves the choice to Claude Code.
    pub fn set_model(&mut self, model: Option<String>) {
        self.claude.model = model;
    }

    /// Tokens the turns of this session used.
    pub fn usage(&self) -> Usage {
        self.transcript.usage
    }

    /// Claude Code's id of the conversation, once a turn reported one.
    pub fn claude_session(&self) -> Option<&str> {
        self.transcript.claude_session.as_deref()
    }

    pub fn turns(&self) -> usize {
        self.transcript.claude_turns
    }

    /// The arguments of one turn. Like a task session (see
    /// [`crate::task::claude_code`]): the [`TOOLS`] are allowed, the deny
    /// rules refuse the dangerous commands and Junees's settings, and the
    /// folder's own Claude Code settings and MCP servers are not loaded.
    /// Unlike it, the permission mode is `default`
    /// (<https://code.claude.com/docs/en/cli-reference>, `--permission-mode`:
    /// "Accepts `default`, ... or `manual` as an alias for `default`"):
    /// a person is at the prompt, but `-p` has nobody to ask, so whatever
    /// would ask is refused (`--permission-prompts none`) and shown. Text
    /// streams as it is written (`--include-partial-messages`).
    ///
    /// The input is not among them: [`Chat::turn`] writes it to stdin,
    /// which `claude -p` reads as the prompt when no prompt argument is
    /// given (<https://code.claude.com/docs/en/headless>, "Pipe data
    /// through Claude": "Non-interactive mode reads stdin"; `--input-format`
    /// is `text` by default). An argument is limited to 128 KiB on Linux,
    /// and one starting with `-` would be read as an option.
    pub fn arguments(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-mode",
            "default",
            "--permission-prompts",
            "none",
            "--tools",
            TOOLS,
            "--allowedTools",
            TOOLS,
            "--setting-sources",
            "user",
            "--strict-mcp-config",
            "--max-turns",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        args.push(self.max_turns.to_string());
        if let Some(id) = self.claude_session() {
            args.push("--resume".into());
            args.push(id.to_string());
        }
        if let Some(model) = &self.claude.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        // Last: the option takes every argument up to the next option.
        let mut deny = deny_rules(&self.settings);
        if let Some(file) = &self.settings.settings_file {
            deny.extend(settings_rules(file, &self.cwd));
        }
        if !deny.is_empty() {
            args.push("--disallowedTools".into());
            args.extend(deny);
        }
        args
    }

    /// Runs one turn, showing its events through `on_event`, and records
    /// it. The cancel flag stops Claude Code and every command it started
    /// ([`Cancelled`]); the next turn continues the same conversation.
    /// Hitting the subscription's usage limit is a [`super::UsageLimit`]
    /// error; a missing or refused login is [`super::NotLoggedIn`].
    pub async fn turn(&mut self, input: &str, mut on_event: impl FnMut(Event<'_>)) -> Result<()> {
        let spawn = Spawn {
            root: &self.cwd,
            args: self.arguments(),
            settings: &self.settings,
            log: None,
            partial: true,
            resumed: self.claude_session().is_some(),
            stdin: Some(input),
        };
        let outcome = self
            .claude
            .session(&spawn, &self.cancel, None, &mut on_event)
            .await;
        self.record(input, &outcome, &mut on_event);
        match outcome.end? {
            End::Answered | End::Budget(_) => Ok(()),
            End::TurnLimit => {
                on_event(Event::Warning(&format!(
                    "Claude Code stopped at --max-turns ({}, JUNEES_MAX_TURNS) before it \
                     finished; say how to go on",
                    self.max_turns
                )));
                Ok(())
            }
            End::Interrupted => Err(Cancelled.into()),
            End::UsageLimit(limit) => Err(limit.into()),
        }
    }

    /// Records a turn. If the journal cannot be written, the conversation
    /// goes on without it, as in [`crate::agent::Agent`].
    fn record(&mut self, input: &str, outcome: &Outcome, on_event: &mut impl FnMut(Event<'_>)) {
        let summary = outcome.summary();
        let end = summary.split(", ").next().unwrap_or_default();
        let entry = Entry::ClaudeTurn(ClaudeTurn {
            claude_session: outcome
                .session_id
                .clone()
                .or_else(|| self.transcript.claude_session.clone()),
            input: input.to_string(),
            text: outcome.answer.clone().unwrap_or_default(),
            usage: outcome.usage,
            end: end.to_string(),
        });
        if let Some(journal) = &mut self.journal
            && let Err(err) = journal.record(&entry)
        {
            self.journal = None;
            on_event(Event::Warning(&format!(
                "could not write the session journal ({err}); \
                 the rest of this session will not be saved"
            )));
        }
        self.transcript.apply(entry);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::super::testing::{claude, events, fake_claude};
    use super::*;
    use crate::session::{CLAUDE_CODE, Session, SessionWriter};
    use crate::task::budget::tokens;
    use crate::tools::SettingsFile;

    /// A journal in memory.
    #[derive(Clone, Default)]
    struct Memory(Arc<Mutex<Vec<Entry>>>);

    impl Journal for Memory {
        fn record(&mut self, entry: &Entry) -> std::io::Result<()> {
            self.0.lock().unwrap().push(entry.clone());
            Ok(())
        }
    }

    fn lines(events: &[Value]) -> String {
        events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The stream-json output of a turn that streams `text` and answers
    /// with it, in Claude Code session `id`.
    fn answer(id: &str, text: &str) -> String {
        let (first, rest) = text.split_at(text.len() / 2);
        lines(&[
            json!({"type": "system", "subtype": "init", "session_id": id}),
            json!({"type": "stream_event", "session_id": id, "parent_tool_use_id": null,
                "event": {"type": "message_start", "message": {"id": format!("m-{text}")}}}),
            json!({"type": "stream_event", "session_id": id, "parent_tool_use_id": null,
                "event": {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": first}}}),
            json!({"type": "stream_event", "session_id": id, "parent_tool_use_id": null,
                "event": {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": rest}}}),
            json!({"type": "stream_event", "session_id": id, "parent_tool_use_id": null,
                "event": {"type": "content_block_stop", "index": 0}}),
            json!({"type": "assistant", "session_id": id, "parent_tool_use_id": null,
                "message": {"id": format!("m-{text}"),
                "content": [{"type": "text", "text": text}],
                "usage": {"input_tokens": 10, "output_tokens": 3}}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": text,
                "session_id": id, "num_turns": 1, "total_cost_usd": 9.0,
                "usage": {"input_tokens": 1000, "output_tokens": 1000}}),
        ])
    }

    /// A fake `claude` that plays `turns` in order, one per run, and saves
    /// each run's arguments as `args-<n>` and its stdin (the prompt) as
    /// `stdin-<n>` in its folder.
    fn fake(dir: &Path, turns: &[String]) -> PathBuf {
        let mut body = String::from(
            "n=$(( $(cat count 2>/dev/null || echo 0) + 1 ))\necho $n > count\n\
             printf '%s\\0' \"$@\" > args-$n\ncat > stdin-$n\ncase $n in\n",
        );
        for (i, turn) in turns.iter().enumerate() {
            body.push_str(&format!("{})\n{turn}\n;;\n", i + 1));
        }
        body.push_str("*) exit 9;;\nesac\n");
        fake_claude(dir, &body)
    }

    fn print(text: &str) -> String {
        format!("cat <<'JSON'\n{text}\nJSON")
    }

    fn args_of(dir: &Path, n: usize) -> Vec<String> {
        std::fs::read(dir.join(format!("args-{n}")))
            .unwrap()
            .split(|&b| b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect()
    }

    fn stdin_of(dir: &Path, n: usize) -> String {
        std::fs::read_to_string(dir.join(format!("stdin-{n}"))).unwrap()
    }

    fn chat(bin: PathBuf, dir: &Path, cancel: Cancel) -> Chat {
        Chat::new(
            claude(bin),
            dir.to_path_buf(),
            ToolSettings::default(),
            9,
            cancel,
        )
    }

    fn turns(journal: &Memory) -> Vec<ClaudeTurn> {
        journal
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|entry| match entry {
                Entry::ClaudeTurn(turn) => turn.clone(),
                other => panic!("unexpected entry {other:?}"),
            })
            .collect()
    }

    #[test]
    fn arguments_ask_nobody_and_keep_the_rules() {
        let file = SettingsFile::new(
            "/home/tester/.junees/config.toml",
            Some(PathBuf::from("/home/tester")),
        );
        let settings = ToolSettings {
            settings_file: Some(file.clone()),
            ..ToolSettings::default()
        };
        let mut chat = Chat::new(
            claude(PathBuf::from("claude")),
            PathBuf::from("/work"),
            settings.clone(),
            7,
            Cancel::new(),
        );
        let args = chat.arguments();
        assert_eq!(
            args[..19],
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-mode",
                "default",
                "--permission-prompts",
                "none",
                "--tools",
                "Read,Edit,Write,Glob,Grep,Bash",
                "--allowedTools",
                "Read,Edit,Write,Glob,Grep,Bash",
                "--setting-sources",
                "user",
                "--strict-mcp-config",
                "--max-turns",
                "7",
                "--disallowedTools",
            ]
        );
        let mut deny = deny_rules(&settings);
        deny.extend(settings_rules(&file, Path::new("/work")));
        assert_eq!(args[19..], deny[..]);
        assert!(args.contains(&"Read(//home/tester/.junees/config.toml)".to_string()));
        for skip in [
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "bypassPermissions",
            "acceptEdits",
            "--bare",
        ] {
            assert!(!args.iter().any(|arg| arg == skip), "{skip}");
        }

        // A later turn continues the conversation, with the model asked for.
        chat.transcript.claude_session = Some("c-1".into());
        chat.set_model(Some("opus".into()));
        let args = chat.arguments();
        assert_eq!(
            args[18..23],
            ["--resume", "c-1", "--model", "opus", "--disallowedTools"]
        );
    }

    #[tokio::test]
    async fn the_second_turn_continues_the_conversation_of_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(
            dir.path(),
            &[
                print(&answer("c-1", "Hello there.")),
                print(&answer("c-1", "Still here.")),
            ],
        );
        let mut chat = chat(bin, dir.path(), Cancel::new());
        let journal = Memory::default();
        chat.reset(Some(Box::new(journal.clone())));
        let seen = Arc::new(Mutex::new(Vec::new()));
        chat.turn("hi", events(&seen)).await.unwrap();
        assert_eq!(chat.claude_session(), Some("c-1"));
        chat.turn("still there?", events(&seen)).await.unwrap();

        // The text came as it was written, once.
        assert_eq!(
            *seen.lock().unwrap(),
            ["text Hello ", "text there.", "text Still", "text  here."]
        );
        let first = args_of(dir.path(), 1);
        assert!(!first.contains(&"--resume".to_string()), "{first:?}");
        // The input goes to stdin, not among the arguments.
        assert_eq!(stdin_of(dir.path(), 1), "hi");
        assert!(!first.contains(&"hi".to_string()), "{first:?}");
        let second = args_of(dir.path(), 2);
        let at = second.iter().position(|arg| arg == "--resume").unwrap();
        assert_eq!(second[at + 1], "c-1");
        assert_eq!(stdin_of(dir.path(), 2), "still there?");

        let turns = turns(&journal);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].claude_session.as_deref(), Some("c-1"));
        assert_eq!(turns[0].input, "hi");
        assert_eq!(turns[0].text, "Hello there.");
        assert_eq!(turns[0].end, "answered");
        // A fresh turn takes the result's totals; a resumed one only what
        // its own stream shows (the totals hold the earlier runs).
        assert_eq!(tokens(turns[0].usage), 2000);
        assert_eq!(tokens(turns[1].usage), 13);
        assert_eq!(tokens(chat.usage()), 2013);
        assert_eq!(chat.turns(), 2);
    }

    #[tokio::test]
    async fn large_and_dash_inputs_reach_claude_intact_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(
            dir.path(),
            &[
                print(&answer("c-8", "Read it.")),
                print(&answer("c-8", "A flag.")),
            ],
        );
        let mut chat = chat(bin, dir.path(), Cancel::new());
        // Over the 128 KiB a single argument may hold on Linux.
        let large = "\u{d55c}\u{ae00} line of text\n".repeat(12_000);
        assert!(large.len() > 200 * 1024);
        chat.turn(&large, |_| {}).await.unwrap();
        assert_eq!(stdin_of(dir.path(), 1), large);
        // Not read as an option: no prompt argument at all.
        chat.turn("-v is a flag", |_| {}).await.unwrap();
        assert_eq!(stdin_of(dir.path(), 2), "-v is a flag");
        let args = args_of(dir.path(), 2);
        assert_eq!(args, chat.arguments());
        assert_eq!(chat.turns(), 2);
    }

    #[tokio::test]
    async fn a_saved_session_continues_its_claude_conversation() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(
            dir.path(),
            &[print(&answer("c-7", "One.")), print(&answer("c-7", "Two."))],
        );
        let (meta, writer) =
            SessionWriter::create_for(home.path(), dir.path(), "", Some(CLAUDE_CODE)).unwrap();
        let mut first = chat(bin.clone(), dir.path(), Cancel::new());
        first.reset(Some(Box::new(writer)));
        first.turn("one", |_| {}).await.unwrap();
        drop(first);

        let path = crate::session::sessions_dir(home.path()).join(format!("{}.jsonl", meta.id));
        let (session, writer) = Session::resume(&path).unwrap();
        let mut second = chat(bin, dir.path(), Cancel::new());
        second.resume(session.transcript, Some(Box::new(writer)));
        second.turn("two", |_| {}).await.unwrap();
        let args = args_of(dir.path(), 2);
        let at = args.iter().position(|arg| arg == "--resume").unwrap();
        assert_eq!(args[at + 1], "c-7");
        drop(second);
        let session = Session::load(&path).unwrap();
        assert_eq!(session.transcript.claude_turns, 2);
        assert_eq!(session.transcript.claude_session.as_deref(), Some("c-7"));
    }

    /// Whether a process is gone (a zombie counts as gone).
    fn gone(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

    async fn wait_for(path: &Path) -> String {
        for _ in 0..400 {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{} never appeared", path.display());
    }

    #[tokio::test]
    async fn cancel_stops_the_turn_and_the_next_turn_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        // Like Claude Code's Bash tool, the command runs detached
        // (`setsid`), out of reach of a signal to the group.
        let hanging = format!(
            "{}\nsetsid sleep 30 &\necho $! > cmd-pid\nwait\nsleep 30",
            print(&lines(&[
                json!({"type": "system", "subtype": "init", "session_id": "c-3"}),
                json!({"type": "stream_event", "session_id": "c-3",
                    "event": {"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "Working"}}}),
            ]))
        );
        let bin = fake(dir.path(), &[hanging, print(&answer("c-3", "Back."))]);
        let cancel = Cancel::new();
        let mut chat = chat(bin, dir.path(), cancel.clone());
        let journal = Memory::default();
        chat.reset(Some(Box::new(journal.clone())));
        let pid_file = dir.path().join("cmd-pid");
        let stop = async {
            wait_for(&pid_file).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(chat.turn("work", |_| {}), stop)
        })
        .await
        .expect("the turn did not stop");
        let err = result.unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        for _ in 0..100 {
            if gone(&pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            gone(&pid),
            "the command Claude Code started is still running"
        );

        cancel.reset();
        chat.turn("go on", |_| {}).await.unwrap();
        let args = args_of(dir.path(), 2);
        let at = args.iter().position(|arg| arg == "--resume").unwrap();
        assert_eq!(args[at + 1], "c-3");
        let turns = turns(&journal);
        assert_eq!(turns[0].end, "interrupted");
        assert_eq!(turns[0].claude_session.as_deref(), Some("c-3"));
        assert_eq!(turns[0].text, "");
        assert_eq!(turns[1].text, "Back.");
    }

    #[tokio::test]
    async fn cancel_stops_a_turn_whose_claude_never_reads_its_input() {
        let dir = tempfile::tempdir().unwrap();
        // A pipe holds 64 KiB: the rest of the input waits for a read
        // that never comes, while the output is read and the turn stops.
        let bin = fake_claude(
            dir.path(),
            &format!(
                "{}\necho started > started\nsleep 30",
                print(&lines(&[
                    json!({"type": "system", "subtype": "init", "session_id": "c-9"})
                ]))
            ),
        );
        let cancel = Cancel::new();
        let mut chat = chat(bin, dir.path(), cancel.clone());
        let input = "x".repeat(1024 * 1024);
        let started = dir.path().join("started");
        let stop = async {
            wait_for(&started).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(chat.turn(&input, |_| {}), stop)
        })
        .await
        .expect("the turn did not stop");
        let err = result.unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
        assert_eq!(chat.claude_session(), Some("c-9"));
    }

    #[tokio::test]
    async fn the_usage_limit_ends_the_turn_with_the_reset_time() {
        let dir = tempfile::tempdir().unwrap();
        let limited = lines(&[
            json!({"type": "system", "subtype": "init", "session_id": "c-4"}),
            json!({"type": "rate_limit_event", "session_id": "c-4", "rate_limit_info":
                {"status": "rejected", "resetsAt": 1_794_205_024}}),
            json!({"type": "result", "subtype": "success", "is_error": true,
                "session_id": "c-4", "api_error_status": 429,
                "result": "You've hit your session limit · resets 6:17am"}),
        ]);
        let bin = fake(dir.path(), &[print(&limited)]);
        let mut chat = chat(bin, dir.path(), Cancel::new());
        let journal = Memory::default();
        chat.reset(Some(Box::new(journal.clone())));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let err = chat.turn("hi", events(&seen)).await.unwrap_err();
        let limit = err.downcast_ref::<super::super::UsageLimit>().unwrap();
        assert_eq!(limit.resets_at, Some(1_794_205_024));
        assert_eq!(
            err.to_string(),
            "Claude Code hit the usage limit (resets 2026-11-09 06:17 UTC): \
             You've hit your session limit · resets 6:17am"
        );
        assert_eq!(turns(&journal)[0].end, "stopped at the usage limit");
        // The conversation can go on once the limit resets.
        assert_eq!(chat.claude_session(), Some("c-4"));
    }
}
