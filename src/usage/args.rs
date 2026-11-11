//! `junees usage` options.

/// `junees usage --help`.
pub const HELP: &str = "\
junees usage [--days N] [--by day|model|folder|source] [--all]

Shows the tokens the saved sessions ($JUNEES_HOME/sessions) used and what
they cost, read from the session journals: the conversation's model calls,
compaction summaries, subagents and Claude Code turns.

Options:
  --days <N>   today and the N-1 days before it (default 7), by the
               `timezone` setting (JUNEES_TIMEZONE; default the system's)
  --by <B>     one row per day (default), model, folder or source
               (terminal, discord, cron, webhook)
  --all        every working folder, not only this one

The cost is an estimate from the Claude API's list prices (the table in
Junees names its source and date); a model without a price is counted but
left out of the cost and named below the table. Set or replace prices with
`prices` in config.toml (JUNEES_PRICES): comma-separated
<model>=<input>/<output>[/<cache write>/<cache read>] in dollars per
million tokens, for example \"claude-opus-4-7=5/25, my-model=1/4\".
Without cache prices a model of the table keeps the table's cache rates
relative to input; other models write at 1.25x input and read at 0.1x. Claude
Code turns (the claude-code backend) run on your subscription: tokens
only. Cache hit is cache read / (cache read + cache write + input).
";

/// Days shown when `--days` is not given.
pub const DEFAULT_DAYS: u32 = 7;

/// The most days `--days` takes.
pub const MAX_DAYS: u32 = 3660;

/// What the rows of `junees usage` stand for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Day,
    Model,
    Folder,
    Source,
}

/// The options of `junees usage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageArgs {
    pub days: u32,
    pub by: By,
    /// Every working folder, not only the current one.
    pub all: bool,
}

impl Default for UsageArgs {
    fn default() -> Self {
        Self {
            days: DEFAULT_DAYS,
            by: By::Day,
            all: false,
        }
    }
}

/// Parses the arguments after `junees usage`; `Ok(None)` asks for help.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<UsageArgs>, String> {
    let mut parsed = UsageArgs::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name.to_string(), Some(value)),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| {
            inline
                .map(str::to_string)
                .or_else(|| args.next())
                .ok_or_else(|| format!("{name} needs {what}"))
        };
        match name.as_str() {
            "-h" | "--help" => return Ok(None),
            "--all" if inline.is_none() => parsed.all = true,
            "--days" => {
                let days = value("a number of days")?;
                parsed.days = days
                    .parse::<u32>()
                    .ok()
                    .filter(|days| (1..=MAX_DAYS).contains(days))
                    .ok_or_else(|| {
                        format!(
                            "--days takes a whole number from 1 to {MAX_DAYS}, not `{}`",
                            crate::prompt_text::escape_hidden(&days)
                        )
                    })?;
            }
            "--by" => {
                let by = value("day, model, folder or source")?;
                parsed.by = match by.as_str() {
                    "day" => By::Day,
                    "model" => By::Model,
                    "folder" => By::Folder,
                    "source" => By::Source,
                    other => {
                        return Err(format!(
                            "--by takes day, model, folder or source, not `{}`",
                            crate::prompt_text::escape_hidden(other)
                        ));
                    }
                };
            }
            _ => {
                return Err(format!(
                    "unknown argument `{}` for junees usage (see junees usage --help)",
                    crate::prompt_text::escape_hidden(&arg)
                ));
            }
        }
    }
    Ok(Some(parsed))
}
