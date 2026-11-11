//! Model prices for the cost estimates of `/usage` and `junees usage`.
//!
//! Prices are kept in thousandths of a US dollar per million tokens
//! ($0.125 / MTok is 125), which is also nano-dollars per token, so every
//! sum stays an exact integer.

use crate::message::Usage;

/// The settings variable that sets or replaces model prices (`prices` in
/// the settings file).
pub const PRICES_ENV: &str = "JUNEES_PRICES";

/// Where [`TABLE`] comes from, and when it was last checked against it.
pub const SOURCE: &str = "https://platform.claude.com/docs/en/about-claude/pricing";
pub const CHECKED: &str = "2026-11-11";

/// What one web search costs, in nano-dollars: $10 per 1,000 searches
/// ([`SOURCE`], "Web search tool").
pub const WEB_SEARCH: u64 = 10_000_000;

/// Prices per million tokens, in thousandths of a dollar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rates {
    pub input: u64,
    pub output: u64,
    /// Writing to the 5-minute prompt cache: Junees marks its cache points
    /// without a `ttl`, which is the 5-minute cache.
    pub cache_write: u64,
    /// Cache hits and refreshes.
    pub cache_read: u64,
}

/// What a model costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub rates: Rates,
    /// Higher rates for a call whose prompt (input, cache writes and cache
    /// reads) is over this many tokens, for models priced by prompt length.
    pub long: Option<(u64, Rates)>,
}

const fn rates(input: u64, cache_write: u64, cache_read: u64, output: u64) -> Rates {
    Rates {
        input,
        output,
        cache_write,
        cache_read,
    }
}

const fn flat(input: u64, cache_write: u64, cache_read: u64, output: u64) -> Price {
    Price {
        rates: rates(input, cache_write, cache_read, output),
        long: None,
    }
}

/// List prices of the Claude API (direct, global routing; not batch, fast
/// mode or US-only inference) by model id without a date suffix, as the
/// "Model pricing" table of
/// https://platform.claude.com/docs/en/about-claude/pricing gave them on
/// 2026-11-11 (columns: base input, 5m cache writes, cache hits, output).
/// Cache hits are 0.1x input except where that table's notes say 0.05x
/// (Opus 5.5, Sonnet 5.5) or 0.025x (Fable/Mythos 5.1 and 5.2). Haiku 5.5
/// costs more for prompts over 100,000 tokens ("Long context pricing").
pub const TABLE: &[(&str, Price)] = &[
    ("claude-fable-5-2", flat(8_000, 10_000, 200, 40_000)),
    ("claude-mythos-5-2", flat(8_000, 10_000, 200, 40_000)),
    ("claude-fable-5-1", flat(10_000, 12_500, 250, 50_000)),
    ("claude-mythos-5-1", flat(10_000, 12_500, 250, 50_000)),
    ("claude-fable-5", flat(10_000, 12_500, 1_000, 50_000)),
    ("claude-mythos-5", flat(10_000, 12_500, 1_000, 50_000)),
    ("claude-opus-5-5", flat(4_000, 5_000, 200, 20_000)),
    ("claude-opus-5", flat(5_000, 6_250, 500, 25_000)),
    ("claude-opus-4-8", flat(5_000, 6_250, 500, 25_000)),
    ("claude-opus-4-7", flat(5_000, 6_250, 500, 25_000)),
    ("claude-opus-4-6", flat(5_000, 6_250, 500, 25_000)),
    ("claude-opus-4-5", flat(5_000, 6_250, 500, 25_000)),
    ("claude-opus-4-1", flat(15_000, 18_750, 1_500, 75_000)),
    ("claude-opus-4", flat(15_000, 18_750, 1_500, 75_000)),
    ("claude-sonnet-5-5", flat(2_000, 2_500, 100, 10_000)),
    ("claude-sonnet-5", flat(2_000, 2_500, 200, 10_000)),
    ("claude-sonnet-4-6", flat(3_000, 3_750, 300, 15_000)),
    ("claude-sonnet-4-5", flat(3_000, 3_750, 300, 15_000)),
    ("claude-sonnet-4", flat(3_000, 3_750, 300, 15_000)),
    (
        "claude-haiku-5-5",
        Price {
            rates: rates(100, 125, 10, 500),
            long: Some((100_000, rates(500, 625, 50, 2_500))),
        },
    ),
    ("claude-haiku-4-5", flat(1_000, 1_250, 100, 5_000)),
];

/// The table, with the prices of the `prices` setting in front.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prices {
    overrides: Vec<(String, Price)>,
}

impl Prices {
    /// The table changed by `setting`, the value of [`PRICES_ENV`]:
    /// comma-separated `<model>=<input>/<output>` with optional
    /// `/<cache write>/<cache read>`, in dollars per million tokens. Left
    /// out, the cache rates keep the table's ratio to input for a model of
    /// the table (cache reads are 0.05x on some), and are 1.25x input for
    /// writes and 0.1x for reads for other models.
    pub fn from_setting(setting: Option<&str>) -> Result<Self, String> {
        let mut overrides = Vec::new();
        for item in setting.unwrap_or_default().split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let fail = |why: &str| {
                format!(
                    "{PRICES_ENV} (`prices`): `{}` {why}; write <model>=<input>/<output>\
                     [/<cache write>/<cache read>] in dollars per million tokens, for \
                     example claude-opus-4-7=5/25",
                    crate::prompt_text::escape_hidden(item)
                )
            };
            let (model, numbers) = item.split_once('=').ok_or_else(|| fail("has no `=`"))?;
            let model = model.trim();
            if model.is_empty() || model.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(fail("names no model"));
            }
            let numbers = numbers
                .split('/')
                .map(|number| parse_dollars(number.trim()))
                .collect::<Option<Vec<u64>>>()
                .ok_or_else(|| fail("has a price that is not a number like 3 or 0.125"))?;
            let price = match numbers[..] {
                [input, output] => {
                    let (write, read) = match Prices::default().get(model) {
                        // The table's cache rates, relative to input.
                        Some(known) => (
                            input * known.rates.cache_write / known.rates.input,
                            input * known.rates.cache_read / known.rates.input,
                        ),
                        None => (input * 5 / 4, input / 10),
                    };
                    flat(input, write, read, output)
                }
                [input, output, write, read] => flat(input, write, read, output),
                _ => return Err(fail("needs 2 or 4 prices")),
            };
            overrides.retain(|(known, _)| known != model);
            overrides.push((model.to_string(), price));
        }
        Ok(Self { overrides })
    }

    /// The price of `model`, if it is known: the setting's for the id as
    /// written or without its date suffix, else the table's.
    pub fn get(&self, model: &str) -> Option<Price> {
        let base = without_date(model);
        let find = |list: &[(&str, Price)]| {
            list.iter()
                .find(|(name, _)| *name == model)
                .or_else(|| list.iter().find(|(name, _)| *name == base))
                .map(|(_, price)| *price)
        };
        let overrides: Vec<(&str, Price)> = self
            .overrides
            .iter()
            .map(|(name, price)| (name.as_str(), *price))
            .collect();
        find(&overrides).or_else(|| find(TABLE))
    }

    /// Whether the setting changed anything.
    pub fn overridden(&self) -> bool {
        !self.overrides.is_empty()
    }
}

/// `claude-haiku-4-5` for `claude-haiku-4-5-20251001`: model ids before
/// the 4.6 generation end in a snapshot date.
fn without_date(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => model,
    }
}

/// Thousandths of a dollar in `3`, `0.125` or `18.75`; at most three
/// decimals and $1,000,000.
fn parse_dollars(text: &str) -> Option<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || whole.len() > 7
        || fraction.len() > 3
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (text.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let mut thousandths: u64 = fraction.parse().unwrap_or(0);
    for _ in fraction.len()..3 {
        thousandths *= 10;
    }
    let dollars: u64 = whole.parse().ok()?;
    (dollars <= 1_000_000).then(|| dollars * 1000 + thousandths)
}

/// What `usage` cost at `price`, in nano-dollars. `one_call` says the
/// usage is that of a single model call, so a prompt-length price applies;
/// sums of several calls (subagents) are priced at the base rates.
pub fn cost(price: &Price, usage: &Usage, one_call: bool) -> u64 {
    let prompt = usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens);
    let rates = match price.long {
        Some((over, long)) if one_call && prompt > over => long,
        _ => price.rates,
    };
    [
        (usage.input_tokens, rates.input),
        (usage.output_tokens, rates.output),
        (usage.cache_creation_input_tokens, rates.cache_write),
        (usage.cache_read_input_tokens, rates.cache_read),
        (usage.web_searches(), WEB_SEARCH),
    ]
    .into_iter()
    .fold(0u64, |sum, (count, rate)| {
        sum.saturating_add(count.saturating_mul(rate))
    })
}

/// `$1.2345`: nano-dollars rounded to a hundredth of a cent.
pub fn dollars(nano: u64) -> String {
    let units = nano.saturating_add(50_000) / 100_000;
    format!("${}.{:04}", units / 10_000, units % 10_000)
}
