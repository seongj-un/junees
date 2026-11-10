//! Approval questions in the channel: a risky tool call posts a question,
//! and the next message of an allowed user in that channel answers it.
//! No answer within the timeout (default 5 minutes) declines the call.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use super::engine::Progress;
use crate::approval::{Answer, Ask, Request, render};

/// Where a channel's next message goes while a question waits.
#[derive(Default)]
pub struct AnswerSlot(Mutex<Option<oneshot::Sender<String>>>);

impl AnswerSlot {
    /// Whether a question waits for its answer.
    pub fn waiting(&self) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
    }

    /// Hands `text` to the waiting question; false when none waits.
    pub fn answer(&self, text: String) -> bool {
        match self.lock().take() {
            Some(sender) => sender.send(text).is_ok(),
            None => false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<oneshot::Sender<String>>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Empties the slot when the question ends, however it ends (a `!stop`
/// drops the question).
struct Clear<'a>(&'a AnswerSlot);

impl Drop for Clear<'_> {
    fn drop(&mut self) {
        self.0.lock().take();
    }
}

/// The [`Ask`] of a channel's conversation.
pub struct DiscordAsk {
    out: UnboundedSender<Progress>,
    slot: Arc<AnswerSlot>,
    timeout: Duration,
}

impl DiscordAsk {
    pub fn new(out: UnboundedSender<Progress>, slot: Arc<AnswerSlot>, timeout: Duration) -> Self {
        Self { out, slot, timeout }
    }
}

#[async_trait]
impl Ask for DiscordAsk {
    async fn ask(&mut self, request: &Request) -> Answer {
        let (sender, answer) = oneshot::channel();
        *self.slot.lock() = Some(sender);
        let _clear = Clear(&self.slot);
        // Through the worker, so it shows after the tool lines before it.
        let _ = self
            .out
            .send(Progress::Question(question(request, self.timeout)));
        match tokio::time::timeout(self.timeout, answer).await {
            Ok(Ok(text)) => parse_answer(&text),
            // The channel's worker is going away.
            Ok(Err(_)) => Answer::No,
            Err(_) => {
                let waited = human(self.timeout);
                let _ = self.out.send(Progress::Note(format!(
                    "(no answer within {waited}: declined)"
                )));
                Answer::NoBecause(format!("no answer within {waited}"))
            }
        }
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
        "**Approve this tool call?**\n```text\n{}```\nReply `y` to run it, `n` to decline \
         (`n <reason>` tells the model why), `a` to allow this kind for the rest of this \
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

    #[tokio::test]
    async fn the_next_message_answers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(AnswerSlot::default());
        let mut ask = DiscordAsk::new(tx, Arc::clone(&slot), Duration::from_secs(5));
        let request = request();
        let asking = ask.ask(&request);
        tokio::pin!(asking);
        tokio::select! {
            _ = &mut asking => panic!("answered without an answer"),
            progress = rx.recv() => assert!(matches!(progress, Some(Progress::Question(_)))),
        }
        assert!(slot.waiting());
        assert!(slot.answer("a".into()));
        assert_eq!(asking.await, Answer::Always);
        assert!(!slot.waiting());
        assert!(!slot.answer("y".into()));
    }

    #[tokio::test]
    async fn no_answer_declines() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Arc::new(AnswerSlot::default());
        let mut ask = DiscordAsk::new(tx, Arc::clone(&slot), Duration::from_secs(1));
        let answer = ask.ask(&request()).await;
        assert_eq!(answer, Answer::NoBecause("no answer within 1s".into()));
        assert!(!slot.waiting());
        assert!(matches!(rx.recv().await, Some(Progress::Question(_))));
        assert_eq!(
            rx.recv().await,
            Some(Progress::Note("(no answer within 1s: declined)".into()))
        );
    }
}
