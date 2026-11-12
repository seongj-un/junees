//! Approval questions in the channel: a risky tool call posts a question,
//! and the next message of an allowed user in that channel answers it, or
//! a press on one of the question's buttons where the platform has them
//! ([`super::platform::Platform::post_question`]). No answer within the
//! timeout (default 5 minutes) declines the call.
//!
//! Every question gets an id of 128 random bits ([`new_id`]) that its
//! buttons carry ([`button_data`]). A press counts only when it names the
//! question that waits in the same conversation (and, once known, comes
//! from the message holding it); a press on an older question, one that
//! timed out or was stopped, or another conversation's question changes
//! nothing ([`AnswerSlot::press`]). The routers check that the person
//! pressing is an allowed user before they get here.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use super::engine::Progress;
use crate::approval::{ASKED_EVERY_TIME, Answer, Ask, Request, render};

/// An approval question as the channel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// What the buttons carry ([`button_data`]).
    pub id: String,
    /// The question as posted ([`super::platform::Platform::question`]).
    pub text: String,
    /// Whether an "always allow" answer means anything: some kind of the
    /// call is not asked about every time.
    pub always: bool,
}

/// A button of a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Allow,
    Always,
    Decline,
}

impl Choice {
    fn letter(self) -> char {
        match self {
            Choice::Allow => 'y',
            Choice::Always => 'a',
            Choice::Decline => 'n',
        }
    }

    /// The buttons of `question`, in the order they show.
    pub fn offered(question: &Question) -> Vec<Choice> {
        if question.always {
            vec![Choice::Allow, Choice::Always, Choice::Decline]
        } else {
            vec![Choice::Allow, Choice::Decline]
        }
    }

    /// The button's label.
    pub fn label(self) -> &'static str {
        match self {
            Choice::Allow => "Allow",
            Choice::Always => "Always allow",
            Choice::Decline => "Decline",
        }
    }
}

/// What a button carries: `junees:<id>:<y|a|n>`, 41 bytes (Discord's
/// `custom_id` takes up to 100 characters, Telegram's `callback_data` up
/// to 64 bytes).
pub fn button_data(id: &str, choice: Choice) -> String {
    format!("{BUTTON_PREFIX}{id}:{}", choice.letter())
}

const BUTTON_PREFIX: &str = "junees:";

/// The question id and choice of a button's data, if it is one of
/// [`button_data`]'s.
pub fn parse_button(data: &str) -> Option<(String, Choice)> {
    let rest = data.strip_prefix(BUTTON_PREFIX)?;
    let (id, letter) = rest.split_once(':')?;
    if id.len() != ID_HEX || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let choice = match letter {
        "y" => Choice::Allow,
        "a" => Choice::Always,
        "n" => Choice::Decline,
        _ => return None,
    };
    Some((id.to_string(), choice))
}

/// Hex digits of a question id.
const ID_HEX: usize = 32;

/// A new question id: 128 bits from the system's random source, in hex.
pub fn new_id() -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; ID_HEX / 2];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        // No random source: a press can never match an id that holds
        // anything but hex, so the question takes text answers only.
        return "unavailable".to_string();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A press on a question's button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    pub id: String,
    pub choice: Choice,
    /// The message the button sits on, as the platform names it.
    pub message: Option<String>,
    /// Who pressed it, as the result line names them.
    pub by: String,
}

/// What answered a question.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    answer: Answer,
    /// Who pressed the button; `None` for a text answer.
    by: Option<String>,
}

/// The question that waits in a channel.
struct Pending {
    id: String,
    always: bool,
    sender: oneshot::Sender<Reply>,
    /// The message holding the buttons, once posted.
    message: Option<String>,
}

/// Where a channel's next message (or a button press) goes while a
/// question waits.
#[derive(Default)]
pub struct AnswerSlot(Mutex<Option<Pending>>);

impl AnswerSlot {
    /// Whether a question waits for its answer.
    pub fn waiting(&self) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(|pending| !pending.sender.is_closed())
    }

    /// Hands `text` to the waiting question; false when none waits.
    pub fn answer(&self, text: String) -> bool {
        match self.lock().take() {
            Some(pending) => pending
                .sender
                .send(Reply {
                    answer: parse_answer(&text),
                    by: None,
                })
                .is_ok(),
            None => false,
        }
    }

    /// Answers the waiting question with a button press; false (and the
    /// question keeps waiting) unless the press names that question, comes
    /// from its message once that is known, and is a button it offered.
    pub fn press(&self, press: Press) -> bool {
        let mut slot = self.lock();
        let matches = slot.as_ref().is_some_and(|pending| {
            pending.id == press.id
                && !pending.sender.is_closed()
                && (pending.message.is_none() || pending.message == press.message)
                && (press.choice != Choice::Always || pending.always)
        });
        if !matches {
            return false;
        }
        let Some(pending) = slot.take() else {
            return false;
        };
        let answer = match press.choice {
            Choice::Allow => Answer::Yes,
            Choice::Always => Answer::Always,
            Choice::Decline => Answer::No,
        };
        pending
            .sender
            .send(Reply {
                answer,
                by: Some(press.by),
            })
            .is_ok()
    }

    /// Notes the message that holds the buttons of question `id`.
    pub fn posted(&self, id: &str, message: String) {
        if let Some(pending) = self.lock().as_mut()
            && pending.id == id
        {
            pending.message = Some(message);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Pending>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Ends the question however it ends (a stop drops the question): empties
/// the slot and says how it ended, so the platform can take the buttons
/// off ([`Progress::Answered`]).
struct Ended<'a> {
    slot: &'a AnswerSlot,
    out: &'a UnboundedSender<Progress>,
    id: String,
    outcome: String,
}

impl Drop for Ended<'_> {
    fn drop(&mut self) {
        self.slot.lock().take();
        let _ = self.out.send(Progress::Answered {
            id: std::mem::take(&mut self.id),
            outcome: std::mem::take(&mut self.outcome),
        });
    }
}

/// The [`Ask`] of a channel's conversation.
pub struct DiscordAsk {
    out: UnboundedSender<Progress>,
    slot: Arc<AnswerSlot>,
    timeout: Duration,
    format: fn(&Request, Duration) -> String,
}

impl DiscordAsk {
    pub fn new(out: UnboundedSender<Progress>, slot: Arc<AnswerSlot>, timeout: Duration) -> Self {
        Self {
            out,
            slot,
            timeout,
            format: question,
        }
    }

    /// Writes the questions with `format` instead of [`question`] (the
    /// platform's spelling: Telegram shows no Markdown).
    pub fn with_format(mut self, format: fn(&Request, Duration) -> String) -> Self {
        self.format = format;
        self
    }
}

#[async_trait]
impl Ask for DiscordAsk {
    async fn ask(&mut self, request: &Request) -> Answer {
        let (sender, answer) = oneshot::channel();
        let id = new_id();
        let always = request
            .kinds
            .iter()
            .any(|kind| !kind.ends_with(ASKED_EVERY_TIME));
        *self.slot.lock() = Some(Pending {
            id: id.clone(),
            always,
            sender,
            message: None,
        });
        let mut ended = Ended {
            slot: &self.slot,
            out: &self.out,
            id: id.clone(),
            outcome: "Not answered: the turn stopped.".to_string(),
        };
        // Through the worker, so it shows after the tool lines before it.
        let _ = self.out.send(Progress::Question(Question {
            id,
            text: (self.format)(request, self.timeout),
            always,
        }));
        match tokio::time::timeout(self.timeout, answer).await {
            Ok(Ok(reply)) => {
                ended.outcome = outcome(&reply);
                reply.answer
            }
            // The channel's worker is going away.
            Ok(Err(_)) => Answer::No,
            Err(_) => {
                let waited = human(self.timeout);
                ended.outcome = format!("No answer within {waited}: declined.");
                let _ = self.out.send(Progress::Note(format!(
                    "(no answer within {waited}: declined)"
                )));
                Answer::NoBecause(format!("no answer within {waited}"))
            }
        }
    }
}

/// The result line of an answered question.
fn outcome(reply: &Reply) -> String {
    let what = match &reply.answer {
        Answer::Yes => "Allowed",
        Answer::Always => "Allowed, and this kind from now on",
        Answer::No => "Declined",
        Answer::NoBecause(_) => "Declined with a reason",
    };
    match &reply.by {
        Some(by) => format!("{what} by {by}."),
        None => format!("{what}."),
    }
}

/// The question as posted: what the call does in a code block, then the
/// choices.
pub fn question(request: &Request, timeout: Duration) -> String {
    let mut details = render(request);
    details.push_str(&format!(
        "     always allows: {}\n",
        request.kinds.join(", ")
    ));
    format!(
        "**Approve this tool call?**\n```text\n{}```\nPress a button, or reply `y` to run it, `n` to \
         decline (`n <reason>` tells the model why), `a` to allow this kind for the rest of this \
         channel's conversation. Anything else declines with your words as the reason. No \
         answer within {} declines.",
        fence_safe(&details),
        human(timeout)
    )
}

/// Text that cannot end the code block it sits in.
pub fn fence_safe(text: &str) -> String {
    text.replace("```", "`\u{200b}`\u{200b}`")
}

/// The answer a reply gives.
pub fn parse_answer(text: &str) -> Answer {
    let text = text.trim();
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let rest = rest.trim();
    match word.to_lowercase().as_str() {
        "y" | "yes" => Answer::Yes,
        "a" | "always" => Answer::Always,
        "n" | "no" if rest.is_empty() => Answer::No,
        "n" | "no" => Answer::NoBecause(rest.to_string()),
        _ if text.is_empty() => Answer::No,
        _ => Answer::NoBecause(text.to_string()),
    }
}

/// `5m`, `90s`, `1h30m`.
pub fn human(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut text = String::new();
    if hours > 0 {
        text.push_str(&format!("{hours}h"));
    }
    if minutes > 0 {
        text.push_str(&format!("{minutes}m"));
    }
    if seconds > 0 || text.is_empty() {
        text.push_str(&format!("{seconds}s"));
    }
    text
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    fn request() -> Request {
        Request {
            tool: "bash".into(),
            target: "rm -r build ```oops".into(),
            reasons: vec!["deletes files".into()],
            preview: None,
            kinds: vec!["bash: delete".into()],
        }
    }

    #[test]
    fn reads_answers() {
        assert_eq!(parse_answer(" Y "), Answer::Yes);
        assert_eq!(parse_answer("yes"), Answer::Yes);
        assert_eq!(parse_answer("a"), Answer::Always);
        assert_eq!(parse_answer("n"), Answer::No);
        assert_eq!(
            parse_answer("n use the tmp folder"),
            Answer::NoBecause("use the tmp folder".into())
        );
        assert_eq!(
            parse_answer("왜 지우려고 해?"),
            Answer::NoBecause("왜 지우려고 해?".into())
        );
        assert_eq!(
            parse_answer("yesterday"),
            Answer::NoBecause("yesterday".into())
        );
    }

    #[test]
    fn the_question_keeps_its_code_block() {
        let text = question(&request(), Duration::from_secs(300));
        assert_eq!(text.matches("```").count(), 2, "{text}");
        assert!(text.contains("approve bash: rm -r build"), "{text}");
        assert!(text.contains("within 5m"), "{text}");
        assert_eq!(human(Duration::from_secs(5400)), "1h30m");
        assert_eq!(human(Duration::from_secs(0)), "0s");
    }

    #[test]
    fn ids_are_random_hex_and_buttons_read_back() {
        let (one, two) = (new_id(), new_id());
        assert_eq!(one.len(), 32);
        assert!(
            one.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_ne!(one, two);
        for choice in [Choice::Allow, Choice::Always, Choice::Decline] {
            let data = button_data(&one, choice);
            // Telegram's callback_data limit.
            assert!(data.len() <= 64, "{data}");
            assert_eq!(parse_button(&data), Some((one.clone(), choice)));
        }
        for data in [
            "",
            "junees:",
            &format!("junees:{one}"),
            &format!("junees:{one}:x"),
            &format!("junees:{one}:yy"),
            &format!("other:{one}:y"),
            &format!("junees:{}:y", &one[1..]),
            &format!("junees:{}:y", one.to_uppercase()),
            "junees:unavailable:y",
        ] {
            assert_eq!(parse_button(data), None, "{data}");
        }
    }

    type Asking<'a> = std::pin::Pin<Box<dyn Future<Output = Answer> + Send + 'a>>;

    /// Asks `request` and returns the question once it is posted.
    async fn posted<'a>(
        ask: &'a mut DiscordAsk,
        request: &'a Request,
        rx: &mut mpsc::UnboundedReceiver<Progress>,
    ) -> (Asking<'a>, Question) {
        let mut asking = ask.ask(request);
        let question = tokio::select! {
            _ = &mut asking => panic!("answered without an answer"),
            progress = rx.recv() => match progress {
                Some(Progress::Question(question)) => question,
                other => panic!("{other:?}"),
            },
        };
        (asking, question)
    }

    fn press(id: &str, choice: Choice, message: Option<&str>) -> Press {
        Press {
            id: id.to_string(),
            choice,
            message: message.map(str::to_string),
            by: "<@1>".to_string(),
        }
    }

    #[tokio::test]
    async fn the_next_message_answers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(AnswerSlot::default());
        let mut ask = DiscordAsk::new(tx, Arc::clone(&slot), Duration::from_secs(5));
        let request = request();
        let (asking, question) = posted(&mut ask, &request, &mut rx).await;
        assert!(question.always);
        assert!(slot.waiting());
        assert!(slot.answer("a".into()));
        assert_eq!(asking.await, Answer::Always);
        assert!(!slot.waiting());
        assert!(!slot.answer("y".into()));
        assert_eq!(
            rx.recv().await,
            Some(Progress::Answered {
                id: question.id.clone(),
                outcome: "Allowed, and this kind from now on.".into()
            })
        );
        // The question is over: its buttons do nothing.
        assert!(!slot.press(press(&question.id, Choice::Allow, None)));
    }

    #[tokio::test]
    async fn only_a_press_on_the_waiting_question_answers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(AnswerSlot::default());
        let mut ask = DiscordAsk::new(tx, Arc::clone(&slot), Duration::from_secs(5));
        let mut request = request();
        // Asked every time: no "always" button.
        request.kinds = vec![format!("bash: merge {ASKED_EVERY_TIME}")];
        let (asking, question) = posted(&mut ask, &request, &mut rx).await;
        assert!(!question.always);
        assert_eq!(Choice::offered(&question), [Choice::Allow, Choice::Decline]);
        slot.posted(&question.id, "77".into());
        // Another question's id, another message, a button not offered.
        assert!(!slot.press(press(&new_id(), Choice::Allow, Some("77"))));
        assert!(!slot.press(press(&question.id, Choice::Allow, Some("78"))));
        assert!(!slot.press(press(&question.id, Choice::Allow, None)));
        assert!(!slot.press(press(&question.id, Choice::Always, Some("77"))));
        assert!(slot.waiting());
        assert!(slot.press(press(&question.id, Choice::Decline, Some("77"))));
        assert_eq!(asking.await, Answer::No);
        assert_eq!(
            rx.recv().await,
            Some(Progress::Answered {
                id: question.id.clone(),
                outcome: "Declined by <@1>.".into()
            })
        );
        assert!(!slot.press(press(&question.id, Choice::Allow, Some("77"))));
    }

    #[tokio::test]
    async fn a_press_answers_before_the_message_is_known() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(AnswerSlot::default());
        let mut ask = DiscordAsk::new(tx, Arc::clone(&slot), Duration::from_secs(5));
        let request = request();
        let (asking, question) = posted(&mut ask, &request, &mut rx).await;
        assert!(slot.press(press(&question.id, Choice::Allow, Some("9"))));
        assert_eq!(asking.await, Answer::Yes);
    }

    #[tokio::test]
    async fn no_answer_declines() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(AnswerSlot::default());
        let mut ask = DiscordAsk::new(tx, Arc::clone(&slot), Duration::from_secs(1));
        let answer = ask.ask(&request()).await;
        assert_eq!(answer, Answer::NoBecause("no answer within 1s".into()));
        assert!(!slot.waiting());
        let Some(Progress::Question(question)) = rx.recv().await else {
            panic!("no question");
        };
        assert_eq!(
            rx.recv().await,
            Some(Progress::Note("(no answer within 1s: declined)".into()))
        );
        assert_eq!(
            rx.recv().await,
            Some(Progress::Answered {
                id: question.id.clone(),
                outcome: "No answer within 1s: declined.".into()
            })
        );
        assert!(!slot.press(press(&question.id, Choice::Allow, None)));
    }

    #[tokio::test]
    async fn a_dropped_question_says_so() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(AnswerSlot::default());
        let mut ask = DiscordAsk::new(tx, Arc::clone(&slot), Duration::from_secs(5));
        let request = request();
        let (asking, question) = posted(&mut ask, &request, &mut rx).await;
        drop(asking);
        assert!(!slot.waiting());
        assert_eq!(
            rx.recv().await,
            Some(Progress::Answered {
                id: question.id.clone(),
                outcome: "Not answered: the turn stopped.".into()
            })
        );
        assert!(!slot.press(press(&question.id, Choice::Allow, None)));
    }
}
