//! Token usage and estimated cost: `/usage` and `junees usage`.
//!
//! Everything comes from the session journals, read and never changed:
//! the `usage` entry of each model call, `summary_usage` of compaction
//! summaries, `subagent_usage` (the tokens of a `delegate` call's
//! subagents, which their own journals in `sessions/<id>/` also hold, so
//! those are not read again) and the usage of `claude_turn` entries.
//! Costs are estimated from [`prices`]; Claude Code turns run on the
//! user's subscription and show tokens only.

pub mod args;
pub mod prices;
pub mod report;
mod scan;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use crate::message::Usage;
pub use prices::{PRICES_ENV, Prices};
pub use report::{Live, period, session};
pub use scan::{Stats, read_session};

/// What a usage record counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// One model call of the conversation (`usage`).
    Call,
    /// A compaction summary call (`summary_usage`).
    Summary,
    /// The calls of a `delegate` call's subagents together
    /// (`subagent_usage`).
    Subagents,
    /// One Claude Code turn (`claude_turn`), on the subscription.
    ClaudeCode,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Call => "model calls",
            Kind::Summary => "summaries",
            Kind::Subagents => "subagents",
            Kind::ClaudeCode => "Claude Code turns",
        }
    }
}

/// One usage entry of a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Unix seconds: when the entry was written, or when the session
    /// started for lines from before journals stamped their lines.
    pub ts: u64,
    pub kind: Kind,
    /// The model of the calls: as the line names it, else the session's
    /// meta line. Empty for a Claude Code session that left the model to
    /// Claude Code.
    pub model: String,
    pub usage: Usage,
}

/// Token sums and what they cost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    pub usage: Usage,
    /// Estimated cost of the tokens whose model has a price, in
    /// nano-dollars.
    pub cost: u64,
    /// Some of the tokens have a price.
    pub priced: bool,
    /// Models whose tokens are counted but have no price, so their cost
    /// is left out.
    pub unpriced: BTreeSet<String>,
    /// Some of the tokens are Claude Code turns, paid by the subscription.
    pub subscription: bool,
}

impl Totals {
    pub fn add(&mut self, record: &Record, prices: &Prices) {
        self.usage.add(record.usage);
        if record.kind == Kind::ClaudeCode {
            self.subscription = true;
            return;
        }
        if record.usage == Usage::default() {
            return;
        }
        match prices.get(&record.model) {
            Some(price) => {
                let one_call = record.kind != Kind::Subagents;
                self.cost = self
                    .cost
                    .saturating_add(prices::cost(&price, &record.usage, one_call));
                self.priced = true;
            }
            None => {
                self.unpriced.insert(model_name(&record.model));
            }
        }
    }

    pub fn merge(&mut self, other: &Totals) {
        self.usage.add(other.usage);
        self.cost = self.cost.saturating_add(other.cost);
        self.priced |= other.priced;
        self.unpriced.extend(other.unpriced.iter().cloned());
        self.subscription |= other.subscription;
    }

    /// Cache reads per thousand prompt tokens (cache reads, cache writes
    /// and uncached input); `None` without any.
    pub fn cache_hit_permille(&self) -> Option<u64> {
        let usage = &self.usage;
        let prompt = usage
            .cache_read_input_tokens
            .saturating_add(usage.cache_creation_input_tokens)
            .saturating_add(usage.input_tokens);
        (prompt > 0).then(|| {
            let reads = u128::from(usage.cache_read_input_tokens) * 1000;
            (reads / u128::from(prompt)) as u64
        })
    }

    /// Every token counted.
    pub fn tokens(&self) -> u64 {
        self.usage.context_tokens()
    }

    /// `$0.1234`, `subscription`, `no price`, or a mix such as
    /// `$0.1234 + no price`.
    pub fn cost_text(&self) -> String {
        let mut parts = Vec::new();
        if self.priced || (self.unpriced.is_empty() && !self.subscription) {
            parts.push(prices::dollars(self.cost));
        }
        if !self.unpriced.is_empty() {
            parts.push("no price".to_string());
        }
        if self.subscription {
            parts.push("subscription".to_string());
        }
        parts.join(" + ")
    }
}

/// A model name as reports show it: hidden characters escaped, and a
/// placeholder for none.
pub fn model_name(model: &str) -> String {
    if model.is_empty() {
        "(model not named)".to_string()
    } else {
        crate::prompt_text::escape_hidden(model)
    }
}
