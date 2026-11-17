//! NCM-style writing (roadmap stage 23): `junees write`, `/write` and the
//! `draft` tool make a long text coarse to fine instead of front to back.
//!
//! The method is borrowed from Next-Concept Modeling (Stanford Hazy
//! Research, arXiv 2610.14162): a text is made in levels, from a coarse
//! plan down to the words, and every position of one level is made at the
//! same time, conditioned only on the earlier levels. Junees applies the
//! method in the harness with the configured model as each level's
//! generator; it does not run an NCM model. See docs/write.md.
//!
//! Levels (`levels`, default 3):
//! 1. the plan: a title and 3 to 12 sections, one concept line each (one
//!    call, JSON; asked once more when the reply is not a valid plan);
//! 2. each section's gist, all sections at once;
//! 3. each section's text, all at once, from the plan and every gist but
//!    no other section's text.
//!
//! With 2 levels the text follows the plan directly; with 4, level 3 is a
//! paragraph outline per section and level 4 writes every paragraph at
//! once. `polish` adds a pass over each seam between two sections.
//!
//! The calls of a level share one semaphore of `parallel` permits. A unit
//! (section, paragraph, seam) whose call fails is asked once more; when
//! that fails too, the earlier level's text stands in for it and the
//! section says so. A cancel ends the run after the last finished level.

pub mod cli;
mod plan;
mod prompts;
#[cfg(test)]
mod tests;
pub mod tool;

pub use plan::{Plan, PlanSection};

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::join_all;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::cancel::Cancel;
use crate::message::{ContentBlock, Message, Role, StopReason, Usage};
use crate::provider::fallback::Switch;
use crate::provider::{CompletionRequest, Provider, StreamEvent};

/// The setting for requests that run at once (`write_parallel`).
pub const PARALLEL_ENV: &str = "JUNEES_WRITE_PARALLEL";
pub const DEFAULT_PARALLEL: usize = 4;
pub const MAX_PARALLEL: usize = 8;
/// The most requests at once of a `draft` call in a run nobody watches.
pub const UNATTENDED_PARALLEL: usize = 2;
pub const DEFAULT_LEVELS: u8 = 3;
pub const MIN_SECTIONS: usize = 3;
pub const MAX_SECTIONS: usize = 12;
/// Paragraphs of one section's outline (`levels` 4).
pub const MAX_PARAGRAPHS: usize = 6;
/// Characters of a request.
pub const MAX_REQUEST_CHARS: usize = 20_000;

const PLAN_TOKENS: u32 = 2_000;
const GIST_TOKENS: u32 = 1_000;
const OUTLINE_TOKENS: u32 = 800;
const PARAGRAPH_TOKENS: u32 = 1_500;
const POLISH_TOKENS: u32 = 2_000;

/// The language of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ko,
    En,
}

impl Lang {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ko" => Ok(Lang::Ko),
            "en" => Ok(Lang::En),
            other => Err(format!("--lang must be ko or en, got `{other}`")),
        }
    }

    /// Korean when the request has Hangul in it, else English.
    pub fn of(request: &str) -> Self {
        if request.chars().any(|c| {
            ('\u{AC00}'..='\u{D7A3}').contains(&c) || ('\u{3131}'..='\u{318E}').contains(&c)
        }) {
            Lang::Ko
        } else {
            Lang::En
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::Ko => "ko",
            Lang::En => "en",
        }
    }
}

/// How a draft is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// 2, 3 or 4.
    pub levels: u8,
    /// Requests at once, 1 to [`MAX_PARALLEL`].
    pub parallel: usize,
    pub polish: bool,
    /// `None`: from the request ([`Lang::of`]).
    pub lang: Option<Lang>,
    /// Output tokens of one section's text (the `max_tokens` setting).
    pub max_tokens: u32,
}

impl Options {
    pub fn new(parallel: usize, max_tokens: u32) -> Self {
        Self {
            levels: DEFAULT_LEVELS,
            parallel,
            polish: false,
            lang: None,
            max_tokens,
        }
    }
}

/// `--levels`: 2, 3 or 4.
pub fn parse_levels(raw: &str) -> Result<u8, String> {
    match raw.trim() {
        "2" => Ok(2),
        "3" => Ok(3),
        "4" => Ok(4),
        other => Err(format!("levels must be 2, 3 or 4, got `{other}`")),
    }
}

/// `--parallel` and `write_parallel`: a whole number from 1 to
/// [`MAX_PARALLEL`].
pub fn parse_parallel(raw: &str) -> Result<usize, String> {
    match raw.trim().parse::<usize>() {
        Ok(value) if (1..=MAX_PARALLEL).contains(&value) => Ok(value),
        _ => Err(format!(
            "parallel must be a whole number from 1 to {MAX_PARALLEL}, got `{}`",
            raw.trim()
        )),
    }
}

/// Reads [`PARALLEL_ENV`] (`write_parallel`), [`DEFAULT_PARALLEL`] when
/// unset.
pub fn read_parallel(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<usize> {
    match get(PARALLEL_ENV) {
        None => Ok(DEFAULT_PARALLEL),
        Some(raw) => parse_parallel(&raw)
            .map_err(|err| anyhow::anyhow!("{PARALLEL_ENV} (`write_parallel`): {err}")),
    }
}

/// What one level makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Concept,
    Gist,
    Outline,
    Body,
    Paragraph,
    Polish,
}

impl Kind {
    pub fn code(self) -> &'static str {
        match self {
            Kind::Concept => "concept",
            Kind::Gist => "gist",
            Kind::Outline => "outline",
            Kind::Body => "body",
            Kind::Paragraph => "paragraph",
            Kind::Polish => "polish",
        }
    }

    /// The level's name for people.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Concept => "개념",
            Kind::Gist => "요지",
            Kind::Outline => "문단 개요",
            Kind::Body => "본문",
            Kind::Paragraph => "문단",
            Kind::Polish => "이음",
        }
    }
}

/// One model call of a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub level: u8,
    pub kind: Kind,
    /// The section (`s2`), paragraph (`s2#1`) or seam (`s2|s3`); `None`
    /// for the plan.
    pub unit: Option<String>,
    /// The model that answered, as the provider names it.
    pub model: Option<String>,
    pub usage: Usage,
    pub elapsed: Duration,
    /// Gave usable text.
    pub ok: bool,
}

/// One section as far as it got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionDraft {
    pub id: String,
    pub concept: String,
    pub gist: Option<String>,
    /// Paragraph outline (`levels` 4).
    pub outline: Vec<String>,
    pub body: Option<String>,
    /// Levels at which the earlier level's text stood in for a failed
    /// call.
    pub failed: Vec<u8>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    Done,
    /// Cancelled: the draft holds the levels finished before.
    Cancelled,
    /// Failed before any text was made (bad request, no plan).
    Failed(String),
    /// The run's token limit ([`Budget`]) stopped it, with the limit's
    /// reason: the draft holds the levels finished before, and the calls
    /// left were not made.
    Limit(String),
}

impl Stop {
    /// `done`, `cancelled`, `failed` or `limit`: how the dashboard names
    /// it (`draft_done`, the session API's `draft`).
    pub fn code(&self) -> &'static str {
        match self {
            Stop::Done => "done",
            Stop::Cancelled => "cancelled",
            Stop::Failed(_) => "failed",
            Stop::Limit(_) => "limit",
        }
    }
}

/// Asked before each call with the tokens the draft's finished calls used;
/// a reason stops the draft there ([`Stop::Limit`]). The agent passes its
/// run's limit, with the tokens used before the draft added.
pub type Budget = dyn Fn(Usage) -> Option<String> + Send + Sync;

/// The result of [`run`].
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub title: String,
    pub sections: Vec<SectionDraft>,
    pub levels: u8,
    /// Levels finished: 0 when not even the plan was made.
    pub levels_done: u8,
    pub polished: bool,
    pub lang: Lang,
    pub stop: Stop,
    pub calls: Vec<Call>,
    pub elapsed: Duration,
    /// The first switch to the fallback model while the draft ran
    /// ([`crate::provider::fallback::Switching`]), to report once at the
    /// end.
    pub switch: Option<Switch>,
}

/// Reported after each finished level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelDone {
    pub level: u8,
    pub kind: Kind,
    pub title: String,
    /// Each section's text at this level, by id.
    pub sections: Vec<(String, String)>,
}

pub type ProgressSender = tokio::sync::mpsc::UnboundedSender<LevelDone>;

/// Checks a request before any call: not empty, at most
/// [`MAX_REQUEST_CHARS`].
pub fn check_request(request: &str) -> Result<(), String> {
    let count = request.trim().chars().count();
    if count == 0 {
        return Err("the request is empty".to_string());
    }
    if count > MAX_REQUEST_CHARS {
        return Err(format!(
            "the request is {count} characters long; at most {MAX_REQUEST_CHARS} are allowed"
        ));
    }
    Ok(())
}

/// Makes a draft of `request` through `provider`. Never fails as a whole:
/// [`Draft::stop`] says how it ended, and the calls made are in
/// [`Draft::calls`] either way. `budget` is asked before each call.
pub async fn run(
    provider: &dyn Provider,
    request: &str,
    options: &Options,
    cancel: &Cancel,
    progress: Option<&ProgressSender>,
    budget: Option<&Budget>,
) -> Draft {
    let started = Instant::now();
    let request = request.trim();
    let lang = options.lang.unwrap_or_else(|| Lang::of(request));
    let mut draft = Draft {
        title: String::new(),
        sections: Vec::new(),
        levels: options.levels.clamp(2, 4),
        levels_done: 0,
        polished: false,
        lang,
        stop: Stop::Done,
        calls: Vec::new(),
        elapsed: Duration::ZERO,
        switch: None,
    };
    if let Err(reason) = check_request(request) {
        draft.stop = Stop::Failed(reason);
        return draft;
    }
    let parallel = options.parallel.clamp(1, MAX_PARALLEL);
    let calls = Calls {
        provider,
        permits: Semaphore::new(parallel),
        parallel,
        made: Mutex::default(),
        budget,
        halted: Mutex::default(),
        switches: Mutex::default(),
    };
    let stop = tokio::select! {
        biased;
        _ = cancel.cancelled() => Stop::Cancelled,
        result = levels(&calls, request, options, lang, &mut draft, progress) => match result {
            Ok(()) => Stop::Done,
            Err(reason) => Stop::Failed(reason),
        },
    };
    let Calls {
        made,
        halted,
        switches,
        ..
    } = calls;
    let halted = halted.into_inner().unwrap_or_else(PoisonError::into_inner);
    draft.stop = match (stop, halted) {
        (Stop::Cancelled, _) => Stop::Cancelled,
        (_, Some(reason)) => Stop::Limit(reason),
        (stop, None) => stop,
    };
    draft.calls = made.into_inner().unwrap_or_else(PoisonError::into_inner);
    draft.switch = switches
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .next();
    draft.elapsed = started.elapsed();
    draft
}

/// The calls of one run: the provider, the permits and what was made.
struct Calls<'a> {
    provider: &'a dyn Provider,
    permits: Semaphore,
    parallel: usize,
    made: Mutex<Vec<Call>>,
    budget: Option<&'a Budget>,
    /// Why the budget stopped the draft; no call is made after it is set.
    halted: Mutex<Option<String>>,
    /// Switches to the fallback model the calls reported.
    switches: Mutex<Vec<Switch>>,
}

/// One request of a level.
struct Ask<'s> {
    level: u8,
    kind: Kind,
    unit: Option<String>,
    system: &'s str,
    user: String,
    max_tokens: u32,
    cache: bool,
}

impl Calls<'_> {
    /// One call; the reply's text, or why it is not usable.
    async fn once(&self, ask: &Ask<'_>, messages: &[Message]) -> Result<String, String> {
        let permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| "closed".to_string())?;
        // Checked once the call may go: calls that ended while this one
        // waited for a permit count too.
        self.check_budget()?;
        let started = Instant::now();
        // Streaming only to hear about switches and which model answered;
        // the text is taken from the response.
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let result = self
            .provider
            .complete_streaming(
                CompletionRequest {
                    system: ask.system,
                    messages,
                    tools: &[],
                    max_tokens: ask.max_tokens,
                    cache: ask.cache,
                    thinking: false,
                    web_search: false,
                },
                &events,
            )
            .await;
        let elapsed = started.elapsed();
        drop(permit);
        let mut answered = None;
        while let Ok(event) = received.try_recv() {
            match event {
                StreamEvent::ModelSwitch(switch) => self
                    .switches
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(switch),
                StreamEvent::AnsweredBy(model) => answered = Some(model),
                _ => {}
            }
        }
        let (usage, text) = match result {
            Ok(response) => {
                let text = response
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
                    .trim()
                    .to_string();
                let text = match response.stop_reason {
                    StopReason::Refusal => Err("the model refused".to_string()),
                    _ if text.is_empty() => Err("the reply was empty".to_string()),
                    _ => Ok(text),
                };
                (response.usage, text)
            }
            Err(err) => (Usage::default(), Err(err.to_string())),
        };
        self.made
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Call {
                level: ask.level,
                kind: ask.kind,
                unit: ask.unit.clone(),
                // The model that answered this call, not the one another
                // call running at the same time left the provider on.
                model: answered.or_else(|| self.provider.model().map(str::to_string)),
                usage,
                elapsed,
                ok: text.is_ok(),
            });
        text
    }

    /// Fails when the draft was stopped, or the budget stops it now with
    /// the tokens the calls so far used.
    fn check_budget(&self) -> Result<(), String> {
        let mut halted = self.halted.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(reason) = halted.as_ref() {
            return Err(reason.clone());
        }
        let Some(budget) = self.budget else {
            return Ok(());
        };
        let mut used = Usage::default();
        for call in self
            .made
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            used.add(call.usage);
        }
        match budget(used) {
            Some(reason) => {
                *halted = Some(reason.clone());
                Err(reason)
            }
            None => Ok(()),
        }
    }

    fn halted(&self) -> bool {
        self.halted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// A unit's text: asked once more when the first call fails or its
    /// reply does not pass `valid`; `None` when both fail.
    async fn unit(&self, ask: Ask<'_>, valid: fn(&str) -> bool) -> Option<String> {
        let messages = [Message::user_text(ask.user.clone())];
        for _ in 0..2 {
            match self.once(&ask, &messages).await {
                Ok(text) if valid(&text) => return Some(text),
                _ => {}
            }
        }
        None
    }

    /// Caching pays only when a later request of the level reads what an
    /// earlier one wrote: the requests that start together all miss it.
    fn cache_for(&self, units: usize) -> bool {
        units > self.parallel
    }
}

fn any_text(_: &str) -> bool {
    true
}

fn valid_outline(text: &str) -> bool {
    !outline_points(text).is_empty()
}

fn valid_seam(text: &str) -> bool {
    seam_edit(text).is_some()
}

/// The lines of a paragraph outline, without list marks; at most
/// [`MAX_PARAGRAPHS`].
pub(crate) fn outline_points(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| {
            line.trim()
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')', '-', '*', ' '])
                .trim()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .take(MAX_PARAGRAPHS)
        .collect()
}

/// A seam edit's `{end, start}`.
fn seam_edit(text: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(plan::object_text(text)?).ok()?;
    let end = value.get("end")?.as_str()?.trim().to_string();
    let start = value.get("start")?.as_str()?.trim().to_string();
    (!end.is_empty() && !start.is_empty()).then_some((end, start))
}

/// The paragraphs of a text: blocks between blank lines.
pub(crate) fn paragraphs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        out.push(current.join("\n"));
    }
    out
}

/// An edited seam paragraph may fix, not rewrite: it must not grow past
/// twice the original and a little.
fn edit_fits(original: &str, edited: &str) -> bool {
    edited.chars().count() <= original.chars().count() * 2 + 200
}

fn report(progress: Option<&ProgressSender>, draft: &Draft, level: u8, kind: Kind) {
    let Some(progress) = progress else {
        return;
    };
    let sections = draft
        .sections
        .iter()
        .map(|section| (section.id.clone(), section.text_at(kind)))
        .collect();
    let _ = progress.send(LevelDone {
        level,
        kind,
        title: draft.title.clone(),
        sections,
    });
}

async fn levels(
    calls: &Calls<'_>,
    request: &str,
    options: &Options,
    lang: Lang,
    draft: &mut Draft,
    progress: Option<&ProgressSender>,
) -> Result<(), String> {
    let plan = make_plan(calls, request, lang).await?;
    draft.title = plan.title.clone();
    draft.sections = plan
        .sections
        .iter()
        .map(|section| SectionDraft {
            id: section.id.clone(),
            concept: section.concept.clone(),
            gist: None,
            outline: Vec::new(),
            body: None,
            failed: Vec::new(),
        })
        .collect();
    draft.levels_done = 1;
    report(progress, draft, 1, Kind::Concept);
    let count = plan.sections.len();
    let levels = draft.levels;
    let mut earlier: Vec<(&str, String)> = Vec::new();

    if levels >= 3 {
        let system = prompts::shared_system(lang, request, &plan, &earlier);
        let cache = calls.cache_for(count);
        let gists = join_all((0..count).map(|i| {
            calls.unit(
                Ask {
                    level: 2,
                    kind: Kind::Gist,
                    unit: Some(plan.sections[i].id.clone()),
                    system: &system,
                    user: prompts::gist_user(&plan, i),
                    max_tokens: GIST_TOKENS,
                    cache,
                },
                any_text,
            )
        }))
        .await;
        // A stopped level is dropped whole, like a cancelled one.
        if calls.halted() {
            return Ok(());
        }
        for (section, gist) in draft.sections.iter_mut().zip(gists) {
            section.gist = Some(gist.unwrap_or_else(|| {
                section.failed.push(2);
                section.concept.clone()
            }));
        }
        draft.levels_done = 2;
        report(progress, draft, 2, Kind::Gist);
        let gists: Vec<String> = draft
            .sections
            .iter()
            .map(|section| section.gist.clone().unwrap_or_default())
            .collect();
        earlier.push(("gists", prompts::gists_text(&plan, &gists)));
    }

    if levels == 4 {
        let system = prompts::shared_system(lang, request, &plan, &earlier);
        let cache = calls.cache_for(count);
        let outlines = join_all((0..count).map(|i| {
            calls.unit(
                Ask {
                    level: 3,
                    kind: Kind::Outline,
                    unit: Some(plan.sections[i].id.clone()),
                    system: &system,
                    user: prompts::outline_user(&plan, i),
                    max_tokens: OUTLINE_TOKENS,
                    cache,
                },
                valid_outline,
            )
        }))
        .await;
        // A stopped level is dropped whole, like a cancelled one.
        if calls.halted() {
            return Ok(());
        }
        for (section, outline) in draft.sections.iter_mut().zip(outlines) {
            section.outline = match outline {
                Some(text) => outline_points(&text),
                None => {
                    section.failed.push(3);
                    vec![section.gist.clone().unwrap_or_default()]
                }
            };
        }
        draft.levels_done = 3;
        report(progress, draft, 3, Kind::Outline);
        let outlines: Vec<Vec<String>> = draft
            .sections
            .iter()
            .map(|section| section.outline.clone())
            .collect();
        earlier.push(("outlines", prompts::outlines_text(&plan, &outlines)));

        let system = prompts::shared_system(lang, request, &plan, &earlier);
        let units: Vec<(usize, usize)> = outlines
            .iter()
            .enumerate()
            .flat_map(|(i, points)| (0..points.len()).map(move |j| (i, j)))
            .collect();
        let cache = calls.cache_for(units.len());
        let written = join_all(units.iter().map(|&(i, j)| {
            calls.unit(
                Ask {
                    level: 4,
                    kind: Kind::Paragraph,
                    unit: Some(format!("{}#{}", plan.sections[i].id, j + 1)),
                    system: &system,
                    user: prompts::paragraph_user(&plan, i, j, &outlines[i][j]),
                    max_tokens: PARAGRAPH_TOKENS.min(options.max_tokens),
                    cache,
                },
                any_text,
            )
        }))
        .await;
        // A stopped level is dropped whole, like a cancelled one.
        if calls.halted() {
            return Ok(());
        }
        let mut bodies: Vec<Vec<String>> = vec![Vec::new(); count];
        for (&(i, j), paragraph) in units.iter().zip(written) {
            let text = paragraph.unwrap_or_else(|| {
                if !draft.sections[i].failed.contains(&4) {
                    draft.sections[i].failed.push(4);
                }
                outlines[i][j].clone()
            });
            bodies[i].push(text);
        }
        for (section, body) in draft.sections.iter_mut().zip(bodies) {
            section.body = Some(body.join("\n\n"));
        }
        draft.levels_done = 4;
        report(progress, draft, 4, Kind::Paragraph);
    } else {
        let with_gists = levels == 3;
        let system = prompts::shared_system(lang, request, &plan, &earlier);
        let cache = calls.cache_for(count);
        let bodies = join_all((0..count).map(|i| {
            calls.unit(
                Ask {
                    level: levels,
                    kind: Kind::Body,
                    unit: Some(plan.sections[i].id.clone()),
                    system: &system,
                    user: prompts::body_user(&plan, i, with_gists),
                    max_tokens: options.max_tokens,
                    cache,
                },
                any_text,
            )
        }))
        .await;
        // A stopped level is dropped whole, like a cancelled one.
        if calls.halted() {
            return Ok(());
        }
        for (section, body) in draft.sections.iter_mut().zip(bodies) {
            section.body = Some(body.unwrap_or_else(|| {
                section.failed.push(levels);
                section
                    .gist
                    .clone()
                    .unwrap_or_else(|| section.concept.clone())
            }));
        }
        draft.levels_done = levels;
        report(progress, draft, levels, Kind::Body);
    }

    if options.polish && count >= 2 {
        polish(calls, lang, draft).await;
        if calls.halted() {
            return Ok(());
        }
        draft.polished = true;
        report(progress, draft, levels + 1, Kind::Polish);
    }
    Ok(())
}

/// Level 1, asked once more when the reply is not a valid plan.
async fn make_plan(calls: &Calls<'_>, request: &str, lang: Lang) -> Result<Plan, String> {
    let ask = Ask {
        level: 1,
        kind: Kind::Concept,
        unit: None,
        system: &prompts::plan_system(lang, request),
        user: prompts::PLAN_USER.to_string(),
        max_tokens: PLAN_TOKENS,
        cache: false,
    };
    let mut messages = vec![Message::user_text(ask.user.clone())];
    let mut last_error = String::new();
    for attempt in 0..2 {
        match calls.once(&ask, &messages).await {
            Ok(reply) => match plan::parse(&reply) {
                Ok(plan) => return Ok(plan),
                Err(reason) => {
                    if attempt == 0 {
                        messages.push(Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::text(reply)],
                        });
                        messages.push(Message::user_text(prompts::plan_retry(&reason)));
                    }
                    last_error = format!("the model gave no valid plan: {reason}");
                }
            },
            Err(reason) => last_error = format!("the plan call failed: {reason}"),
        }
    }
    Err(last_error)
}

/// The seam pass: one call per pair of neighbouring sections, all at once,
/// each seeing the last paragraph before the seam and the first after it.
/// An edit replaces only those two paragraphs. A one-paragraph section
/// sits on two seams; the first seam's edit of it wins.
async fn polish(calls: &Calls<'_>, lang: Lang, draft: &mut Draft) {
    let system = prompts::polish_system(lang);
    let mut texts: Vec<Vec<String>> = draft
        .sections
        .iter()
        .map(|section| paragraphs(section.body.as_deref().unwrap_or_default()))
        .collect();
    let seams: Vec<usize> = (0..texts.len().saturating_sub(1))
        .filter(|&i| !texts[i].is_empty() && !texts[i + 1].is_empty())
        .collect();
    let cache = calls.cache_for(seams.len());
    let edits = join_all(seams.iter().map(|&i| {
        let before = &draft.sections[i];
        let after = &draft.sections[i + 1];
        calls.unit(
            Ask {
                level: draft.levels + 1,
                kind: Kind::Polish,
                unit: Some(format!("{}|{}", before.id, after.id)),
                system: &system,
                user: prompts::polish_user(
                    &before.id,
                    texts[i].last().expect("checked above"),
                    &after.id,
                    &texts[i + 1][0],
                ),
                max_tokens: POLISH_TOKENS,
                cache,
            },
            valid_seam,
        )
    }))
    .await;
    if calls.halted() {
        return;
    }
    // The sections whose first paragraph a seam already changed.
    let mut start_changed = vec![false; texts.len()];
    for (&i, edit) in seams.iter().zip(edits) {
        let Some((end, start)) = edit.as_deref().and_then(seam_edit) else {
            continue;
        };
        let last = texts[i].len() - 1;
        if !(last == 0 && start_changed[i]) && edit_fits(&texts[i][last], &end) {
            texts[i][last] = end;
        }
        if edit_fits(&texts[i + 1][0], &start) {
            texts[i + 1][0] = start;
            start_changed[i + 1] = true;
        }
    }
    for (section, text) in draft.sections.iter_mut().zip(texts) {
        if section.body.is_some() {
            section.body = Some(text.join("\n\n"));
        }
    }
}

impl SectionDraft {
    /// The section's text as level `kind` left it.
    pub fn text_at(&self, kind: Kind) -> String {
        match kind {
            Kind::Concept => self.concept.clone(),
            Kind::Gist => self.gist.clone().unwrap_or_default(),
            Kind::Outline => self
                .outline
                .iter()
                .enumerate()
                .map(|(i, point)| format!("{}. {point}", i + 1))
                .collect::<Vec<_>>()
                .join("\n"),
            Kind::Body | Kind::Paragraph | Kind::Polish => self.body.clone().unwrap_or_default(),
        }
    }

    /// The most detailed text the section has.
    pub fn best_text(&self) -> String {
        if let Some(body) = &self.body {
            body.clone()
        } else if !self.outline.is_empty() {
            self.text_at(Kind::Outline)
        } else if let Some(gist) = &self.gist {
            gist.clone()
        } else {
            self.concept.clone()
        }
    }
}

/// Usage of one level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelUsage {
    pub level: u8,
    pub kind: Kind,
    pub calls: usize,
    pub failed_calls: usize,
    pub usage: Usage,
    /// The calls' time added up.
    pub call_time: Duration,
}

impl Draft {
    /// The draft as Markdown: the title, then each section under its
    /// concept, with the most detailed text it has. A section whose text
    /// stands in for a failed level says so.
    pub fn text(&self) -> String {
        if self.levels_done == 0 {
            return String::new();
        }
        let mut out = format!("# {}\n", self.title);
        for section in &self.sections {
            out.push_str(&format!("\n## {}\n", section.concept));
            // A section with only its concept has nothing under the heading.
            if section.gist.is_some() || section.body.is_some() || !section.outline.is_empty() {
                out.push_str(&format!("\n{}\n", section.best_text()));
            }
            if !section.failed.is_empty() {
                out.push_str(&format!("\n> {}\n", self.failure_note(section)));
            }
        }
        out
    }

    fn failure_note(&self, section: &SectionDraft) -> String {
        let levels = section
            .failed
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        match self.lang {
            Lang::Ko => format!(
                "[{}] 이 절은 {levels}단계 생성에 실패해 앞 단계의 글을 대신 넣었습니다.",
                section.id
            ),
            Lang::En => format!(
                "[{}] Level {levels} of this section failed; the earlier level's text stands \
                 in for it.",
                section.id
            ),
        }
    }

    /// Ids of the sections where an earlier level's text stands in.
    pub fn failed_sections(&self) -> Vec<&str> {
        self.sections
            .iter()
            .filter(|section| !section.failed.is_empty())
            .map(|section| section.id.as_str())
            .collect()
    }

    /// Every call's tokens added up.
    pub fn usage(&self) -> Usage {
        let mut total = Usage::default();
        for call in &self.calls {
            total.add(call.usage);
        }
        total
    }

    /// Every call's tokens added up per model that answered, in the order
    /// the models first answered; calls that used no tokens are left out.
    pub fn usage_by_model(&self) -> Vec<(Option<String>, Usage)> {
        let mut models: Vec<(Option<String>, Usage)> = Vec::new();
        for call in self
            .calls
            .iter()
            .filter(|call| call.usage != Usage::default())
        {
            match models.iter_mut().find(|(model, _)| *model == call.model) {
                Some((_, usage)) => usage.add(call.usage),
                None => models.push((call.model.clone(), call.usage)),
            }
        }
        models
    }

    /// The calls' time added up: about what the draft would have taken
    /// with one call at a time.
    pub fn sequential_estimate(&self) -> Duration {
        self.calls.iter().map(|call| call.elapsed).sum()
    }

    /// [`Self::sequential_estimate`] where the comparison means something:
    /// the draft finished ([`Stop::Done`]) and one call at a time would have
    /// taken longer than it did, as far as the lines show it (tenths of a
    /// second, so a line never reads "0.1s, about 0.1s"). A draft that
    /// stopped early leaves out the calls it dropped while `elapsed` counts
    /// the time they ran, so the sum would read shorter than the draft took.
    pub fn sequential_comparison(&self) -> Option<Duration> {
        let estimate = self.sequential_estimate();
        (self.stop == Stop::Done && tenths(estimate) > tenths(self.elapsed)).then_some(estimate)
    }

    /// Usage per level, in level order.
    pub fn by_level(&self) -> Vec<LevelUsage> {
        let mut levels: Vec<LevelUsage> = Vec::new();
        for call in &self.calls {
            let entry = match levels.iter_mut().find(|entry| entry.level == call.level) {
                Some(entry) => entry,
                None => {
                    levels.push(LevelUsage {
                        level: call.level,
                        kind: call.kind,
                        calls: 0,
                        failed_calls: 0,
                        usage: Usage::default(),
                        call_time: Duration::ZERO,
                    });
                    levels.last_mut().expect("just pushed")
                }
            };
            entry.calls += 1;
            entry.failed_calls += usize::from(!call.ok);
            entry.usage.add(call.usage);
            entry.call_time += call.elapsed;
        }
        levels.sort_by_key(|entry| entry.level);
        levels
    }

    /// What the calls cost by `prices`, in nano dollars; `None` when a
    /// call's model has no price.
    pub fn cost(&self, prices: &crate::usage::Prices) -> Option<u64> {
        let mut total = 0;
        for call in &self.calls {
            let price = prices.get(call.model.as_deref()?)?;
            total += crate::usage::prices::cost(&price, &call.usage, true);
        }
        Some(total)
    }

    /// One line: calls, tokens, cost (when `prices` are given), time taken
    /// and the one-at-a-time estimate when it compares
    /// ([`Self::sequential_comparison`]).
    pub fn summary(&self, prices: Option<&crate::usage::Prices>) -> String {
        let usage = self.usage();
        let input =
            usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;
        let cost = match prices.map(|prices| self.cost(prices)) {
            None => String::new(),
            Some(Some(nano)) => format!(", 비용 {}", crate::usage::prices::dollars(nano)),
            Some(None) => ", 비용 알 수 없음".to_string(),
        };
        format!(
            "호출 {}번, 입력 {} / 출력 {} 토큰{cost}, 걸린 시간 {}{}",
            self.calls.len(),
            crate::usage::report::thousands(input),
            crate::usage::report::thousands(usage.output_tokens),
            seconds(self.elapsed),
            self.sequential_comparison()
                .map(|estimate| format!(", 순차였다면 약 {}", seconds(estimate)))
                .unwrap_or_default(),
        )
    }
}

/// `2단계 요지 완료 (6개 절)`: one line about a finished level.
pub fn level_line(level: &LevelDone) -> String {
    format!(
        "{}단계 {} 완료 ({}개 절)",
        level.level,
        level.kind.label(),
        level.sections.len()
    )
}

/// `12.3초`.
pub fn seconds(duration: Duration) -> String {
    format!("{}초", tenths_text(duration))
}

/// A duration in tenths of a second, rounded half up: the precision the
/// summary and the status line show, and so the one comparisons use.
fn tenths(duration: Duration) -> u128 {
    (duration.as_millis() + 50) / 100
}

/// `12.3`: seconds to the tenth ([`tenths`]).
pub(crate) fn tenths_text(duration: Duration) -> String {
    let tenths = tenths(duration);
    format!("{}.{}", tenths / 10, tenths % 10)
}
