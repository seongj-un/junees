//! Reading the stream-json output of a Claude Code session: the events
//! to show, the tokens used, and how the session ended.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{
    AUTH_ERRORS, ERROR_CHARS, End, Exit, NotLoggedIn, Outcome, RATE_LIMIT_ERROR, Sink, UsageLimit,
    looks_like_login_failure, warn,
};
use crate::agent::Event;
use crate::cli::preview;
use crate::message::Usage;
use crate::task::budget::tokens;
use crate::tools::ToolOutput;

/// What the stream-json output of a session said so far.
#[derive(Default)]
pub(super) struct Stream {
    /// Tool names by tool_use id, for showing the results.
    tool_names: HashMap<String, String>,
    /// The usage of each model response, by message id: Claude Code sends
    /// a response's blocks as separate events, each with the usage.
    usage: HashMap<String, Usage>,
    /// Usage of responses without an id.
    unnamed: Usage,
    session_id: Option<String>,
    /// What a refused login said, if the latest model call failed so.
    auth: Option<String>,
    /// What Claude Code said, if the latest model call failed with
    /// [`RATE_LIMIT_ERROR`].
    rate_limited: Option<String>,
    /// The latest `rate_limit_event`.
    rate_limit: Option<RateLimit>,
    /// Whether a response's usage could not be read (warned once).
    bad_usage: bool,
    result: Option<Finished>,
    /// The last lines that were not JSON.
    plain: Vec<String>,
}

/// The `rate_limit_info` of a `rate_limit_event`
/// (<https://code.claude.com/docs/en/agent-sdk/typescript>,
/// `SDKRateLimitEvent`).
struct RateLimit {
    /// `status` is `rejected`: requests are refused.
    rejected: bool,
    /// `resetsAt`, in unix seconds (the docs give no unit; the 2.1.319
    /// binary documents it as "unix epoch seconds", the unit of the
    /// status line's `rate_limits.*.resets_at`).
    resets_at: Option<u64>,
    /// `errorCode` is `credits_required`: the included usage is used up and
    /// only bought usage credits let the session go on.
    credits_required: bool,
}

/// The `result` event.
struct Finished {
    subtype: String,
    is_error: bool,
    /// `api_error_status`: the HTTP status of the API error that ended the
    /// session.
    api_error_status: Option<u64>,
    text: String,
    usage: Option<Usage>,
    cost_usd: Option<f64>,
    turns: Option<u64>,
}

impl Stream {
    /// Takes one line of output; returns whether the usage changed.
    pub(super) fn handle(&mut self, line: &str, report: &mut Sink<'_>) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            warn(report, &format!("claude: {}", preview(line, ERROR_CHARS)));
            if self.plain.len() == 20 {
                self.plain.remove(0);
            }
            self.plain.push(line.to_string());
            return false;
        };
        if let Some(id) = event["session_id"].as_str() {
            self.session_id = Some(id.to_string());
        }
        match event["type"].as_str() {
            Some("assistant") => return self.assistant(&event, report),
            Some("user") => self.user(&event, report),
            Some("system") => self.system(&event, line, report),
            Some("result") => self.result = Some(Finished::parse(&event)),
            Some("rate_limit_event") => self.rate_limit_event(&event, report),
            _ => {}
        }
        false
    }

    fn assistant(&mut self, event: &Value, report: &mut Sink<'_>) -> bool {
        let message = &event["message"];
        let main = event["parent_tool_use_id"].is_null();
        // Only the latest model call counts: a refused login that a later
        // response or another error followed is not why the session ended.
        self.auth = match event["error"].as_str() {
            Some(error) if AUTH_ERRORS.contains(&error) => {
                let text = content_text(&message["content"]);
                Some(if text.is_empty() {
                    error.to_string()
                } else {
                    text
                })
            }
            _ => None,
        };
        // A response that came through means requests are not refused.
        if event["error"].is_null() {
            self.rate_limit = None;
        }
        self.rate_limited = (event["error"] == RATE_LIMIT_ERROR).then(|| {
            let text = content_text(&message["content"]);
            if text.trim().is_empty() {
                RATE_LIMIT_ERROR.to_string()
            } else {
                text
            }
        });
        let mut changed = false;
        if let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) {
            match serde_json::from_value::<Usage>(usage.clone()) {
                Ok(usage) => {
                    match message["id"].as_str() {
                        Some(id) => {
                            self.usage.insert(id.to_string(), usage);
                        }
                        None => self.unnamed.add(usage),
                    }
                    changed = true;
                }
                Err(err) if !self.bad_usage => {
                    self.bad_usage = true;
                    warn(
                        report,
                        &format!(
                            "cannot read the token usage of a Claude Code response ({err}); \
                             the budget does not count such responses"
                        ),
                    );
                }
                Err(_) => {}
            }
        }
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") if main => {
                    let text = block["text"].as_str().unwrap_or_default().trim();
                    if !text.is_empty() {
                        // Claude Code hands over whole blocks.
                        report(Event::TextDelta(text));
                        report(Event::TextEnd);
                    }
                }
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or("?");
                    if let Some(id) = block["id"].as_str() {
                        self.tool_names.insert(id.to_string(), name.to_string());
                    }
                    if main {
                        report(Event::ToolCall {
                            name,
                            input: &block["input"],
                        });
                    }
                }
                _ => {}
            }
        }
        changed
    }

    fn user(&mut self, event: &Value, report: &mut Sink<'_>) {
        if !event["parent_tool_use_id"].is_null() {
            return;
        }
        for block in event["message"]["content"].as_array().into_iter().flatten() {
            if block["type"] != "tool_result" {
                continue;
            }
            let name = block["tool_use_id"]
                .as_str()
                .and_then(|id| self.tool_names.get(id))
                .map_or("?", String::as_str);
            let output = ToolOutput {
                content: content_text(&block["content"]),
                is_error: block["is_error"].as_bool().unwrap_or(false),
            };
            report(Event::ToolResult {
                name,
                output: &output,
            });
        }
    }

    fn system(&mut self, event: &Value, line: &str, report: &mut Sink<'_>) {
        match event["subtype"].as_str() {
            Some("api_retry") => {
                let error = event["error"].as_str().unwrap_or("unknown");
                self.auth = AUTH_ERRORS.contains(&error).then(|| error.to_string());
                self.rate_limited = (error == RATE_LIMIT_ERROR).then(|| error.to_string());
                warn(
                    report,
                    &format!(
                        "Claude Code retries a model call (attempt {} of {}): {error}",
                        event["attempt"], event["max_retries"]
                    ),
                );
            }
            Some("permission_denied") => warn(
                report,
                &format!(
                    "Claude Code refused a tool call: {}",
                    preview(line, ERROR_CHARS)
                ),
            ),
            _ => {}
        }
    }

    fn rate_limit_event(&mut self, event: &Value, report: &mut Sink<'_>) {
        let info = &event["rate_limit_info"];
        let limit = RateLimit {
            rejected: info["status"] == "rejected",
            resets_at: info["resetsAt"]
                .as_f64()
                .filter(|at| at.is_finite() && *at > 0.0)
                .map(|at| at as u64),
            credits_required: info["errorCode"] == "credits_required",
        };
        if !limit.rejected {
            // The limit let a request through.
            self.rate_limited = None;
        } else {
            let when = limit.resets_at.map_or_else(String::new, |at| {
                format!(" until {} UTC", crate::session::format_utc(at))
            });
            warn(
                report,
                &format!("Claude Code reports the usage limit reached; requests are refused{when}"),
            );
        }
        self.rate_limit = Some(limit);
    }

    /// Tokens of the model responses seen so far.
    pub(super) fn usage(&self) -> Usage {
        let mut total = self.unnamed;
        for usage in self.usage.values() {
            total.add(*usage);
        }
        total
    }

    pub(super) fn outcome(self, exit: Result<Exit>) -> Outcome {
        let streamed = self.usage();
        // The result's totals also count calls the stream does not show;
        // the stream counts responses of a session that never got there.
        let usage = match self.result.as_ref().and_then(|result| result.usage) {
            Some(reported) if tokens(reported) >= tokens(streamed) => reported,
            _ => streamed,
        };
        let end = exit.and_then(|exit| self.end(exit));
        let (cost_usd, turns) = self
            .result
            .as_ref()
            .map_or((None, None), |result| (result.cost_usd, result.turns));
        Outcome {
            end,
            usage,
            cost_usd,
            turns,
            session_id: self.session_id,
        }
    }

    fn end(&self, exit: Exit) -> Result<End> {
        if let Some(stopped) = exit.stopped {
            return Ok(stopped);
        }
        match &self.result {
            Some(result) if !result.is_error && result.subtype == "success" => {
                return Ok(End::Answered);
            }
            Some(result) if result.subtype == "error_max_turns" => return Ok(End::TurnLimit),
            _ => {}
        }
        if let Some(said) = self.login_failure(&exit.stderr) {
            return Err(NotLoggedIn(said).into());
        }
        if let Some(limit) = self.usage_limit() {
            return Ok(End::UsageLimit(limit));
        }
        let stderr = if exit.stderr.is_empty() {
            String::new()
        } else {
            format!("; stderr: {}", preview(&exit.stderr, ERROR_CHARS))
        };
        match &self.result {
            Some(result) => Err(anyhow!(
                "Claude Code ended the session with `{}` ({}{}): {}{stderr}",
                result.subtype,
                exit.status,
                result
                    .api_error_status
                    .map_or_else(String::new, |status| format!(", API status {status}")),
                preview(&result.text, ERROR_CHARS)
            )),
            None => Err(anyhow!(
                "`claude` exited ({}) without a result{stderr}",
                exit.status
            )),
        }
    }

    /// The usage limit, if that is why a session that did not finish
    /// ended: the latest `rate_limit_event` refuses requests, or a 429
    /// (the result's status or a model call failing with
    /// [`RATE_LIMIT_ERROR`]) comes with Claude Code's usage-limit message.
    /// A 429 alone is not enough: the server also throttles for a moment
    /// without any plan limit ("Server is temporarily limiting requests
    /// (not your usage limit)"), and that is an error to look at, not a
    /// reset to wait for.
    fn usage_limit(&self) -> Option<UsageLimit> {
        let refused = self.rate_limit.as_ref().filter(|limit| limit.rejected);
        let status_429 = self
            .result
            .as_ref()
            .is_some_and(|result| result.api_error_status == Some(429));
        let says_limit = || {
            self.result
                .as_ref()
                .map(|result| result.text.as_str())
                .into_iter()
                .chain(self.rate_limited.as_deref())
                .any(says_usage_limit)
        };
        if refused.is_none() && !((self.rate_limited.is_some() || status_429) && says_limit()) {
            return None;
        }
        let said = self
            .result
            .as_ref()
            .map(|result| result.text.trim())
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .or_else(|| self.rate_limited.clone())
            .unwrap_or_else(|| "requests refused by the usage limit".to_string());
        let mut message = preview(&said, ERROR_CHARS);
        if refused.is_some_and(|limit| limit.credits_required) {
            message.push_str(
                " (Claude Code says usage credits are required to go on: \
                 `credits_required`, which does not lift by itself)",
            );
        }
        Some(UsageLimit {
            resets_at: refused.and_then(|limit| limit.resets_at),
            message,
        })
    }

    /// What Claude Code said when its login was missing or refused.
    fn login_failure(&self, stderr: &str) -> Option<String> {
        if let Some(said) = &self.auth {
            return Some(said.clone());
        }
        match &self.result {
            Some(result) => (result.is_error && looks_like_login_failure(&result.text))
                .then(|| result.text.clone()),
            None => self
                .plain
                .iter()
                .map(String::as_str)
                .chain([stderr])
                .find(|text| looks_like_login_failure(text))
                .map(str::to_string),
        }
    }
}

impl Finished {
    fn parse(event: &Value) -> Self {
        let text = match event["result"].as_str() {
            Some(text) => text.to_string(),
            None => event["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; "),
        };
        Self {
            subtype: event["subtype"].as_str().unwrap_or("?").to_string(),
            is_error: event["is_error"].as_bool().unwrap_or(false),
            api_error_status: event["api_error_status"].as_u64(),
            text,
            usage: result_usage(event),
            cost_usd: event["total_cost_usd"].as_f64(),
            turns: event["num_turns"].as_u64(),
        }
    }
}

/// Whether `text` is one of Claude Code's usage-limit messages
/// (<https://code.claude.com/docs/en/errors>, "Usage limits"): "You've hit
/// your session limit", "... weekly limit", "... monthly spend limit",
/// "... team's shared budget", "Usage credits required ...".
fn says_usage_limit(text: &str) -> bool {
    let text = text.replace('\u{2019}', "'").to_lowercase();
    text.contains("usage credits required")
        || text.split("you've hit your").skip(1).any(|rest| {
            let what = rest.split(['.', '\n', '\u{b7}']).next().unwrap_or("");
            what.contains("limit") || what.contains("budget")
        })
}

/// The tokens a result reports: `modelUsage` counts every model the
/// session used (subagents and Claude Code's own small calls too),
/// `usage` only the main conversation.
fn result_usage(event: &Value) -> Option<Usage> {
    if let Some(models) = event["modelUsage"].as_object()
        && !models.is_empty()
    {
        let mut total = Usage::default();
        for model in models.values() {
            let count = |key: &str| model[key].as_u64().unwrap_or(0);
            total.add(Usage {
                input_tokens: count("inputTokens"),
                output_tokens: count("outputTokens"),
                cache_creation_input_tokens: count("cacheCreationInputTokens"),
                cache_read_input_tokens: count("cacheReadInputTokens"),
                ..Default::default()
            });
        }
        return Some(total);
    }
    event
        .get("usage")
        .filter(|usage| usage.is_object())
        .and_then(|usage| serde_json::from_value(usage.clone()).ok())
}

/// The text of a message's or tool result's content: a string, or the
/// text of its blocks.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block["type"].as_str() {
                Some("text") => block["text"].as_str().unwrap_or_default().to_string(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::process::ExitStatus;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::super::testing::{SESSION, events as collector};
    use super::*;

    #[test]
    fn the_stream_shows_events_and_counts_each_response_once() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut report = collector(&seen);
        let mut stream = Stream::default();
        let mut changed = Vec::new();
        for line in SESSION.lines() {
            changed.push(stream.handle(line, &mut report));
        }
        assert_eq!(changed, [false, true, true, false, true, false]);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "text Looking.",
                "-> Read {\"file_path\":\"a.txt\"}",
                "<- Read false hello",
                "text Done.",
            ]
        );
        assert_eq!(tokens(stream.usage()), 115 + 27);
        let exit = Exit {
            status: ExitStatus::default(),
            stopped: None,
            stderr: String::new(),
        };
        let outcome = stream.outcome(Ok(exit));
        assert_eq!(
            outcome.summary(),
            "answered, 2 turns, 187 tokens, estimated cost $0.0123, claude session s-1"
        );
        // modelUsage counts the small model too.
        assert_eq!(
            outcome.usage,
            Usage {
                input_tokens: 70,
                output_tokens: 15,
                cache_creation_input_tokens: 2,
                cache_read_input_tokens: 100,
                ..Default::default()
            }
        );
        assert_eq!(outcome.turns, Some(2));
        assert_eq!(outcome.cost_usd, Some(0.0123));
        assert_eq!(outcome.session_id.as_deref(), Some("s-1"));
        assert_eq!(outcome.end.unwrap(), End::Answered);
    }

    #[test]
    fn warns_once_about_usage_it_cannot_read() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut report = collector(&seen);
        let mut stream = Stream::default();
        for id in ["msg_1", "msg_2"] {
            let event = json!({"type": "assistant", "parent_tool_use_id": null,
                "message": {"id": id, "content": [],
                "usage": {"input_tokens": null, "output_tokens": 3}}});
            assert!(!stream.handle(&event.to_string(), &mut report));
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(
            seen[0].starts_with("warn cannot read the token usage of a Claude Code response"),
            "{seen:?}"
        );
    }

    fn end_of(lines: &[Value], stderr: &str) -> Result<End> {
        let mut stream = Stream::default();
        for line in lines {
            stream.handle(&line.to_string(), &mut |_| {});
        }
        stream
            .outcome(Ok(Exit {
                status: ExitStatus::default(),
                stopped: None,
                stderr: stderr.into(),
            }))
            .end
    }

    #[test]
    fn tells_login_failures_from_other_failures() {
        let not_logged_in = json!({"type": "result", "subtype": "success", "is_error": true,
            "result": "Not logged in · Please run /login"});
        let err = end_of(&[not_logged_in], "").unwrap_err();
        assert!(err.downcast_ref::<NotLoggedIn>().is_some(), "{err:#}");
        assert!(
            format!("{err:#}").contains("run `claude` and then /login"),
            "{err:#}"
        );

        let refused = json!({"type": "assistant", "error": "authentication_failed",
            "parent_tool_use_id": null, "message": {"content": [{"type": "text",
            "text": "Failed to authenticate. API Error: 401"}]}});
        let failed =
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true});
        let err = end_of(&[refused, failed.clone()], "").unwrap_err();
        assert!(
            format!("{err:#}").contains("(Failed to authenticate. API Error: 401)"),
            "{err:#}"
        );
        assert!(err.downcast_ref::<NotLoggedIn>().is_some());

        let err = end_of(&[], "Invalid API key · Fix external API key").unwrap_err();
        assert!(err.downcast_ref::<NotLoggedIn>().is_some(), "{err:#}");

        // Only the latest failure counts: a refused login, then another
        // error or a response, is no login failure.
        let retry = |error: &str| {
            json!({"type": "system", "subtype": "api_retry", "attempt": 1,
                "max_retries": 10, "error": error})
        };
        let answer = json!({"type": "assistant", "parent_tool_use_id": null,
            "message": {"content": [{"type": "text", "text": "hi"}]}});
        for later in [retry("overloaded"), answer] {
            let err = end_of(
                &[
                    retry("authentication_failed"),
                    later.clone(),
                    failed.clone(),
                ],
                "",
            )
            .unwrap_err();
            assert!(err.downcast_ref::<NotLoggedIn>().is_none(), "{later}");
        }
        for error in ["oauth_org_not_allowed", "account_on_hold"] {
            let err = end_of(&[retry("overloaded"), retry(error), failed.clone()], "").unwrap_err();
            assert!(err.downcast_ref::<NotLoggedIn>().is_some(), "{error}");
        }

        // Retries that end in success are no failure.
        let retry = retry("authentication_failed");
        let success =
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "ok"});
        assert_eq!(end_of(&[retry, success], "").unwrap(), End::Answered);

        let turns = json!({"type": "result", "subtype": "error_max_turns", "is_error": true});
        assert_eq!(end_of(&[turns], "").unwrap(), End::TurnLimit);

        let err = end_of(&[failed], "boom").unwrap_err();
        assert!(err.downcast_ref::<NotLoggedIn>().is_none());
        assert!(
            format!("{err:#}")
                .starts_with("Claude Code ended the session with `error_during_execution`"),
            "{err:#}"
        );
        let err = end_of(&[], "segfault").unwrap_err();
        assert!(
            format!("{err:#}").contains("without a result; stderr: segfault"),
            "{err:#}"
        );
    }

    #[test]
    fn tells_the_usage_limit_from_other_failures() {
        // As Claude Code reports a refused request: the event with the
        // reset time, the model call's error, then the result.
        let refused = |resets_at: Value| {
            json!({"type": "rate_limit_event", "session_id": "s",
                "rate_limit_info": {"status": "rejected", "resetsAt": resets_at}})
        };
        let call = json!({"type": "assistant", "error": "rate_limit",
            "parent_tool_use_id": null, "message": {"content": [{"type": "text",
            "text": "You've hit your session limit · resets 3:45pm"}]}});
        let result = json!({"type": "result", "subtype": "success", "is_error": true,
            "api_error_status": 429, "result": "You've hit your session limit · resets 3:45pm"});
        let limit = |resets_at| {
            End::UsageLimit(UsageLimit {
                resets_at,
                message: "You've hit your session limit · resets 3:45pm".into(),
            })
        };
        assert_eq!(
            end_of(
                &[refused(json!(1_794_205_024)), call.clone(), result.clone()],
                ""
            )
            .unwrap(),
            limit(Some(1_794_205_024))
        );
        // The event, or a 429 with the limit message, is enough; only the
        // event has the reset time.
        assert_eq!(
            end_of(
                &[
                    call.clone(),
                    json!({"type": "result", "subtype": "error_during_execution",
                "is_error": true})
                ],
                ""
            )
            .unwrap(),
            limit(None)
        );
        assert_eq!(
            end_of(std::slice::from_ref(&result), "").unwrap(),
            limit(None)
        );
        let End::UsageLimit(alone) = end_of(&[refused(json!(1_794_205_024))], "").unwrap() else {
            panic!("not a usage limit");
        };
        assert_eq!(alone.resets_at, Some(1_794_205_024));
        assert_eq!(alone.message, "requests refused by the usage limit");
        // A 429 without Claude Code's limit message is the server's short
        // throttle, not the plan limit: an error showing the 429.
        let throttled = "API Error: Server is temporarily limiting requests (not your usage limit)";
        let err = end_of(
            &[
                json!({"type": "system", "subtype": "api_retry", "attempt": 10,
                "max_retries": 10, "error": "rate_limit"}),
                json!({"type": "assistant", "error": "rate_limit", "parent_tool_use_id": null,
                "message": {"content": [{"type": "text", "text": throttled}]}}),
                json!({"type": "result", "subtype": "success", "is_error": true,
                "api_error_status": 429, "result": throttled}),
            ],
            "",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("API status 429"), "{err:#}");
        assert!(
            end_of(
                &[
                    json!({"type": "system", "subtype": "api_retry", "attempt": 3,
                "max_retries": 10, "error": "rate_limit"})
                ],
                "",
            )
            .is_err()
        );
        assert!(says_usage_limit("You\u{2019}ve hit your weekly limit"));
        assert!(says_usage_limit("You've hit your team's shared budget"));
        assert!(!says_usage_limit("You've hit your stride · 429"));
        // A reset time Junees cannot read is none.
        let End::UsageLimit(odd) = end_of(&[refused(json!("3:45pm")), result.clone()], "").unwrap()
        else {
            panic!("not a usage limit");
        };
        assert_eq!(odd.resets_at, None);
        // Only bought credits help.
        let credits = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected",
            "errorCode": "credits_required", "canUserPurchaseCredits": true}});
        let End::UsageLimit(credits) = end_of(&[credits, result.clone()], "").unwrap() else {
            panic!("not a usage limit");
        };
        assert!(
            credits.message.contains("`credits_required`"),
            "{}",
            credits.message
        );

        // A session that got past the limit, or a limit that lifted, is no
        // usage limit; neither is an overloaded server.
        let success =
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "ok"});
        assert_eq!(
            end_of(&[refused(json!(1)), call.clone(), success], "").unwrap(),
            End::Answered
        );
        let answer = json!({"type": "assistant", "parent_tool_use_id": null,
            "message": {"content": [{"type": "text", "text": "hi"}]}});
        let failed =
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true});
        let allowed = json!({"type": "rate_limit_event",
            "rate_limit_info": {"status": "allowed_warning", "resetsAt": 1_794_205_024}});
        for later in [answer, allowed] {
            let err = end_of(
                &[
                    call.clone(),
                    refused(json!(1)),
                    later.clone(),
                    failed.clone(),
                ],
                "",
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("error_during_execution"),
                "{later}"
            );
        }
        let overloaded = json!({"type": "assistant", "error": "overloaded",
            "parent_tool_use_id": null, "message": {"content": []}});
        assert!(end_of(&[overloaded, failed], "").is_err());
    }

    #[test]
    fn the_stream_warns_when_the_limit_refuses_requests() {
        let mut stream = Stream::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut report = collector(&seen);
        for status in ["allowed", "rejected"] {
            let event = json!({"type": "rate_limit_event", "rate_limit_info":
                {"status": status, "resetsAt": 1_794_205_024}});
            stream.handle(&event.to_string(), &mut report);
        }
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "warn Claude Code reports the usage limit reached; requests are refused \
                 until 2026-11-09 06:17 UTC"
            ]
        );
    }

    #[test]
    fn a_usage_limit_names_the_reset() {
        let outcome = Outcome {
            end: Ok(End::UsageLimit(UsageLimit {
                resets_at: Some(1_794_205_024),
                message: "You've hit your session limit · resets 6:17am".into(),
            })),
            usage: Usage::default(),
            cost_usd: None,
            turns: None,
            session_id: None,
        };
        assert!(
            outcome
                .summary()
                .starts_with("stopped at the usage limit, ")
        );
        let Ok(End::UsageLimit(limit)) = outcome.end else {
            unreachable!()
        };
        assert_eq!(
            limit.to_string(),
            "Claude Code hit the usage limit (resets 2026-11-09 06:17 UTC): \
             You've hit your session limit · resets 6:17am"
        );
    }
}
