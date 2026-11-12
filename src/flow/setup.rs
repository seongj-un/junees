//! `junees webhook add-github`: one command that sets up a flow route. It
//! makes a random secret in `$JUNEES_HOME/secrets/`, the default prompt
//! files, the route (its events and GitHub writes follow the flow), and
//! says what to enter on GitHub and which settings are still missing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::FlowConfig;
use crate::cron::deliver::{Delivery, Lookup};
use crate::cron::store::{create_private_dir, write_private};
use crate::github::{Repo, Write};
use crate::webhook::store::{Kind, Route, Secret, Store, check_name};

/// The prompt stored on a flow route; flows use `flows/*.md` instead.
pub const FLOW_PROMPT: &str =
    "(github flow: the prompts are $JUNEES_HOME/flows/review.md and address.md)";

/// `junees webhook add-github`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AddGithubArgs {
    pub name: String,
    pub repo: Option<String>,
    pub review: bool,
    pub address_reviews: bool,
    pub merge_when_ready: bool,
    pub dir: Option<PathBuf>,
    pub deliver: Option<String>,
    pub test_cmd: Option<String>,
    pub min_approvals: Option<String>,
    pub allow_self_review: bool,
    pub re_review: bool,
    pub max_runs_per_hour: Option<String>,
    /// `--approver` values as given (repeatable, comma-separated).
    pub approvers: Vec<String>,
    /// `--require-check` values (repeatable; names may hold commas).
    pub required_checks: Vec<String>,
    /// `--allow-home`: the route may work in the home folder.
    pub allow_home: bool,
}

/// A flow route just added.
#[derive(Debug)]
pub struct Added {
    pub route: Route,
    pub flow: FlowConfig,
    pub secret_file: PathBuf,
    /// Settings still missing, as lines.
    pub missing: Vec<String>,
}

fn number(value: Option<&String>, what: &str, default: u32) -> Result<u32> {
    match value {
        None => Ok(default),
        Some(text) => text
            .trim()
            .parse()
            .with_context(|| format!("{what} takes a number (got `{text}`)")),
    }
}

/// The words of repeatable, comma-separated option values, without
/// repeats (case-insensitive) and empty words.
fn list(values: &[String]) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in values.iter().flat_map(|value| value.split(',')) {
        let word = word.trim();
        if !word.is_empty() && !words.iter().any(|known| known.eq_ignore_ascii_case(word)) {
            words.push(word.to_string());
        }
    }
    words
}

/// Checks `args`, writes the secret and the default prompts, and saves the
/// route.
pub fn add(store: &Store, lookup: &Lookup, args: AddGithubArgs, now: i64) -> Result<Added> {
    check_name(&args.name)?;
    let repo = args
        .repo
        .as_deref()
        .context("`junees webhook add-github` needs --repo owner/name")?;
    let flow = FlowConfig {
        repo: Repo::parse(repo)
            .with_context(|| format!("--repo takes owner/name (got `{repo}`)"))?
            .to_string(),
        review: args.review,
        address_reviews: args.address_reviews,
        merge_when_ready: args.merge_when_ready,
        test_cmd: args.test_cmd.clone(),
        min_approvals: number(args.min_approvals.as_ref(), "--min-approvals", 1)?,
        allow_self_review: args.allow_self_review,
        re_review: args.re_review,
        max_runs_per_hour: number(
            args.max_runs_per_hour.as_ref(),
            "--max-runs-per-hour",
            super::DEFAULT_RUNS_PER_HOUR,
        )?,
        approvers: list(&args.approvers),
        required_checks: args.required_checks.clone(),
    };
    flow.check()?;
    if flow.allow_self_review && !(flow.merge_when_ready && flow.review) {
        bail!("--allow-self-review needs --review and --merge-when-ready");
    }
    if flow.test_cmd.is_some() && !flow.address_reviews {
        bail!("--test-cmd is for --address-reviews");
    }
    let dir = match &args.dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("cannot read the current folder")?,
    };
    let dir = std::fs::canonicalize(&dir)
        .ok()
        .filter(|dir| dir.is_dir())
        .with_context(|| format!("{} is not a folder", dir.display()))?;
    crate::cron::command::check_home_folder(lookup, &dir, args.allow_home, None)?;
    if flow.address_reviews {
        let repo = Repo::parse(&flow.repo).expect("checked");
        super::address::origin_of(&dir, &repo).context("--address-reviews works in --dir")?;
    }
    let deliver = match &args.deliver {
        Some(text) => Delivery::parse(text)?,
        None => Delivery::Local,
    };
    let home = store.home().to_path_buf();
    let mut locked = store.lock()?;
    if locked.routes.iter().any(|route| route.name == args.name) {
        bail!("a route named `{}` exists already", args.name);
    }
    let secret_file = write_secret(&home, &args.name)?;
    super::prompts::write_defaults(&home)?;
    let route = Route {
        name: args.name.clone(),
        kind: Kind::Github,
        prompt: FLOW_PROMPT.to_string(),
        secret: Some(Secret::File(secret_file.clone())),
        no_secret: false,
        events: flow.events(),
        dir,
        allow_home: args.allow_home,
        deliver,
        paused: false,
        created_at: now,
        last: None,
        last_event: None,
        github_allow: flow.github_allow(),
        flow: Some(flow.clone()),
    };
    locked.routes.push(route.clone());
    locked.save()?;
    drop(locked);
    // Results of an earlier route of this name do not count.
    let mut state = super::state::lock(&home)?;
    state.forget(&args.name);
    state.save()?;
    Ok(Added {
        missing: missing_settings(lookup, &flow),
        route,
        flow,
        secret_file,
    })
}

/// Writes a new random secret (32 bytes, hex) for route `name` to
/// `$JUNEES_HOME/secrets/webhook-<name>` (0600, folder 0700).
pub(crate) fn write_secret(home: &Path, name: &str) -> Result<PathBuf> {
    use ring::rand::SecureRandom;
    let folder = crate::tools::secrets_dir(home);
    create_private_dir(&folder)?;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot make a random secret"))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let path = folder.join(format!("webhook-{name}"));
    write_private(&path, &hex)?;
    Ok(path)
}

/// What the user still has to set for the flow to work, as lines.
pub fn missing_settings(lookup: &Lookup, flow: &FlowConfig) -> Vec<String> {
    let mut missing = Vec::new();
    let settings = match crate::github::Settings::read(|key| {
        lookup(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }) {
        Ok(settings) => settings,
        Err(err) => return vec![format!("the GitHub settings are not readable: {err:#}")],
    };
    if settings.token.is_none() {
        missing.push(
            "github_token (JUNEES_GITHUB_TOKEN): the flow reads and writes GitHub with it"
                .to_string(),
        );
    }
    let repo = Repo::parse(&flow.repo).expect("checked");
    if !settings.repos.contains(&repo) {
        missing.push(format!(
            "github_repos (JUNEES_GITHUB_REPOS) must list {repo}: now {}",
            if settings.repos.is_empty() {
                "empty".to_string()
            } else {
                settings
                    .repos
                    .iter()
                    .map(Repo::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }
    let needed = flow.github_allow();
    let lacking: Vec<Write> = needed
        .iter()
        .copied()
        .filter(|write| !settings.unattended.contains(write))
        .collect();
    if !lacking.is_empty() {
        let mut all = settings.unattended.clone();
        all.extend(lacking.iter().copied());
        missing.push(format!(
            "github_allow_unattended (JUNEES_GITHUB_ALLOW_UNATTENDED) lacks {}: set it to \"{}\"",
            Write::join(&lacking),
            all.iter().map(|w| w.name()).collect::<Vec<_>>().join(",")
        ));
    }
    if flow.merge_when_ready && !super::merge_enabled(&**lookup).unwrap_or(false) {
        missing.push(format!(
            "github_flow_merge ({}) = true: --merge-when-ready merges only with it too",
            super::MERGE_ENV
        ));
    }
    missing
}

/// The text `add-github` prints: the route, what to enter on GitHub, the
/// settings still missing.
pub fn instructions(added: &Added, serving: bool) -> String {
    let route = &added.route;
    let flow = &added.flow;
    let mut out = format!(
        "added GitHub flow route {}: {} on {} (POST /hooks/{})\n",
        route.name,
        flow.actions(),
        flow.repo,
        route.name
    );
    out.push_str(&format!(
        "\nOn GitHub: {} -> Settings -> Webhooks -> Add webhook\n",
        flow.repo
    ));
    out.push_str(&format!(
        "  Payload URL:   https://<your public address>/hooks/{}\n",
        route.name
    ));
    out.push_str("  Content type:  application/json\n");
    out.push_str(&format!(
        "  Secret:        the contents of {} (cat it; it is 0600)\n",
        added.secret_file.display()
    ));
    let mut events = Vec::new();
    if flow.review || flow.address_reviews || flow.merge_when_ready {
        events.push("Pull requests");
    }
    if flow.address_reviews || flow.merge_when_ready {
        events.push("Pull request reviews");
    }
    if flow.merge_when_ready {
        events.push("Check suites");
        events.push("Statuses");
    }
    out.push_str(&format!(
        "  Events:        Let me select individual events: {}\n",
        events.join(", ")
    ));
    if !serving {
        out.push_str(
            "\n`junees webhook serve` is not running: start it and put a tunnel or reverse proxy \
             in front (README, \"웹훅\").\n",
        );
    }
    out.push_str(&format!(
        "\nThe model's GitHub writes on this route: {} (never merge; the flow code merges).\n",
        Write::join(&route.github_allow)
    ));
    out.push_str(
        "Prompts: $JUNEES_HOME/flows/review.md and address.md (edit them to change the reviews).\n",
    );
    if flow.address_reviews && flow.test_cmd.is_none() {
        out.push_str("No --test-cmd: addressed changes are pushed without a test run.\n");
    }
    if !added.missing.is_empty() {
        out.push_str("\nStill needed in the settings (config.toml or the environment):\n");
        for line in &added.missing {
            out.push_str(&format!("  - {line}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::cron::deliver::tests::lookup;

    fn args(name: &str, dir: &Path) -> AddGithubArgs {
        AddGithubArgs {
            name: name.into(),
            repo: Some("o/n".into()),
            review: true,
            dir: Some(dir.to_path_buf()),
            ..AddGithubArgs::default()
        }
    }

    #[test]
    fn adds_a_route_with_a_private_secret_and_the_flows_events() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let settings = lookup(&[]);
        let added = add(&store, &settings, args("pr", home.path()), 5).unwrap();
        let route = &added.route;
        assert_eq!(
            route.events,
            [
                "pull_request.opened",
                "pull_request.synchronize",
                "pull_request.ready_for_review"
            ]
        );
        assert_eq!(route.github_allow, [Write::Review]);
        assert_eq!(route.flow.as_ref().unwrap().repo, "o/n");
        assert!(route.flow.as_ref().unwrap().approvers.is_empty());
        assert_eq!(store.routes().unwrap(), std::slice::from_ref(route));
        let secret = std::fs::read_to_string(&added.secret_file).unwrap();
        assert_eq!(secret.trim().len(), 64, "{secret}");
        assert!(secret.trim().chars().all(|c| c.is_ascii_hexdigit()));
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&added.secret_file), 0o600);
        assert_eq!(mode(&home.path().join("secrets")), 0o700);
        assert_eq!(
            Secret::File(added.secret_file.clone())
                .read(&|_| None)
                .unwrap(),
            secret.trim().as_bytes()
        );
        assert!(home.path().join("flows/review.md").is_file());
        assert!(home.path().join("flows/address.md").is_file());
        // Every setting is missing.
        assert_eq!(added.missing.len(), 3, "{:?}", added.missing);
        let text = instructions(&added, false);
        for line in [
            "added GitHub flow route pr: review on o/n (POST /hooks/pr)\n",
            "Payload URL:   https://<your public address>/hooks/pr\n",
            "Content type:  application/json\n",
            "secrets/webhook-pr (cat it; it is 0600)\n",
            "Events:        Let me select individual events: Pull requests\n",
            "github_allow_unattended (JUNEES_GITHUB_ALLOW_UNATTENDED) lacks review: set it to \"review\"",
            "github_repos (JUNEES_GITHUB_REPOS) must list o/n: now empty",
            "`junees webhook serve` is not running",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
        assert!(!text.contains(&secret.trim().to_string()), "{text}");
        let err = add(&store, &settings, args("pr", home.path()), 5).unwrap_err();
        assert!(err.to_string().contains("exists already"), "{err}");
        // The secret of the route stays.
        assert_eq!(std::fs::read_to_string(&added.secret_file).unwrap(), secret);
    }

    #[test]
    fn approver_lists_take_repeats_and_commas() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let mut args = args("pr", home.path());
        args.merge_when_ready = true;
        args.approvers = vec!["alice, bob".into(), "Alice".into(), "carol,".into()];
        let added = add(&store, &lookup(&[]), args, 5).unwrap();
        assert_eq!(added.flow.approvers, ["alice", "bob", "carol"]);
        // Stored with the route; old routes without the field read empty.
        let stored = store.routes().unwrap();
        assert_eq!(
            stored[0].flow.as_ref().unwrap().approvers,
            ["alice", "bob", "carol"]
        );
    }

    #[test]
    fn says_which_settings_each_flow_still_needs() {
        let flow = FlowConfig {
            review: true,
            address_reviews: true,
            merge_when_ready: true,
            ..super::super::config("o/n")
        };
        let settings = lookup(&[
            ("JUNEES_GITHUB_TOKEN", "t"),
            ("JUNEES_GITHUB_REPOS", "o/other, o/n"),
            ("JUNEES_GITHUB_ALLOW_UNATTENDED", "comment"),
        ]);
        assert_eq!(
            missing_settings(&settings, &flow),
            [
                "github_allow_unattended (JUNEES_GITHUB_ALLOW_UNATTENDED) lacks review: set it to \"comment,review\"",
                "github_flow_merge (JUNEES_GITHUB_FLOW_MERGE) = true: --merge-when-ready merges only with it too",
            ]
        );
        let settings = lookup(&[
            ("JUNEES_GITHUB_TOKEN", "t"),
            ("JUNEES_GITHUB_REPOS", "O/N"),
            ("JUNEES_GITHUB_ALLOW_UNATTENDED", "review,comment,merge"),
            ("JUNEES_GITHUB_FLOW_MERGE", "true"),
        ]);
        assert!(missing_settings(&settings, &flow).is_empty());
    }

    #[test]
    fn refuses_flows_that_cannot_work() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let settings = lookup(&[]);
        for (change, wanted) in [
            (
                Box::new(|a: &mut AddGithubArgs| a.repo = None) as Box<dyn Fn(&mut AddGithubArgs)>,
                "needs --repo",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.repo = Some("o".into())),
                "owner/name",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.review = false),
                "at least one",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.min_approvals = Some("0".into())),
                "--min-approvals takes 1",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.max_runs_per_hour = Some("x".into())),
                "--max-runs-per-hour takes a number",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.allow_self_review = true),
                "--allow-self-review needs",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.test_cmd = Some("make".into())),
                "--test-cmd is for",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| {
                    a.address_reviews = true;
                    a.test_cmd = Some("make\u{1b}".into());
                }),
                "U+001B",
            ),
            // Not a clone of o/n.
            (
                Box::new(|a: &mut AddGithubArgs| a.address_reviews = true),
                "--address-reviews works in --dir",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.name = "A".into()),
                "route name",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| {
                    a.merge_when_ready = true;
                    a.approvers = vec!["alice,../x".into()];
                }),
                "--approver takes GitHub logins (got `../x`)",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.approvers = vec!["alice".into()]),
                "--approver is for",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.required_checks = vec!["ci".into()]),
                "--require-check is for",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| {
                    a.merge_when_ready = true;
                    a.required_checks = vec![" ".into()];
                }),
                "--require-check takes a check name",
            ),
            (
                Box::new(|a: &mut AddGithubArgs| a.deliver = Some("slack".into())),
                "--deliver",
            ),
        ] {
            let mut args = args("pr", home.path());
            change(&mut args);
            let err = format!("{:#}", add(&store, &settings, args, 5).unwrap_err());
            assert!(err.contains(wanted), "{wanted}: {err}");
        }
        assert!(store.routes().unwrap().is_empty());
        assert!(!home.path().join("secrets").exists());
    }
}
