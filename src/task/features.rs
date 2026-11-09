//! The feature list, `.junees/features.json`:
//!
//! ```json
//! [{"id": "F1", "description": "...", "verify": "<shell command>", "passes": false}]
//! ```
//!
//! Ids are unique and every feature has a verify command: a shell command,
//! run from the repository root, that exits 0 once the feature is done.
//! Fields beyond these four are kept as they are.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{features_path, write_atomic};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feature {
    pub id: String,
    pub description: String,
    pub verify: String,
    pub passes: bool,
    /// Any other fields (steps, category, ...), kept unchanged.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Parses a feature list and checks it: a non-empty array, unique non-empty
/// ids, and a non-empty verify command on every feature. Every problem found
/// is listed in the error, so a model can fix them all at once.
pub fn parse(text: &str) -> Result<Vec<Feature>> {
    let features: Vec<Feature> = serde_json::from_str(text)
        .context("features.json must be a JSON array of {id, description, verify, passes}")?;
    if features.is_empty() {
        bail!("features.json lists no features");
    }
    let mut problems = Vec::new();
    let mut seen = HashSet::new();
    for (index, feature) in features.iter().enumerate() {
        let id = feature.id.trim();
        if id.is_empty() {
            problems.push(format!("feature #{} has an empty id", index + 1));
        } else if !seen.insert(id) {
            problems.push(format!("id `{id}` is used more than once"));
        }
        if feature.verify.trim().is_empty() {
            problems.push(format!(
                "feature #{} (`{id}`) has an empty verify command",
                index + 1
            ));
        }
    }
    if !problems.is_empty() {
        bail!("invalid features.json: {}", problems.join("; "));
    }
    Ok(features)
}

/// Checks that no feature passes yet, as in a list a new task starts from.
pub fn check_fresh(features: &[Feature]) -> Result<()> {
    let passing: Vec<&str> = features
        .iter()
        .filter(|feature| feature.passes)
        .map(|feature| feature.id.as_str())
        .collect();
    if !passing.is_empty() {
        bail!(
            "a new task must start with every feature at \"passes\": false, \
             but these are true: {}",
            passing.join(", ")
        );
    }
    Ok(())
}

/// Reads and checks the feature list of the repository at `root`.
pub fn load(root: &Path) -> Result<Vec<Feature>> {
    let path = features_path(root);
    let text =
        fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).with_context(|| path.display().to_string())
}

/// The file content the harness writes for a feature list.
pub fn to_json(features: &[Feature]) -> String {
    let mut text = serde_json::to_string_pretty(features).expect("features always serialize");
    text.push('\n');
    text
}

/// The harness's own copy of the feature list, the only one that counts.
///
/// A model can rewrite `features.json` like any file, so the copy here is
/// the authority: only [`Ledger::mark_passing`] changes it, and
/// [`Ledger::restore`] puts it back into the file.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    features: Vec<Feature>,
    /// The file was found changed by someone other than the harness.
    edited: bool,
}

impl Ledger {
    /// Takes the list in `root` as the authority and writes it back in the
    /// harness's formatting.
    pub fn load(root: &Path) -> Result<Self> {
        let features = load(root)?;
        let ledger = Self {
            path: features_path(root),
            features,
            edited: false,
        };
        ledger.write()?;
        Ok(ledger)
    }

    pub fn features(&self) -> &[Feature] {
        &self.features
    }

    pub fn get(&self, id: &str) -> Option<&Feature> {
        self.features.iter().find(|feature| feature.id == id)
    }

    pub fn passing(&self) -> usize {
        self.features
            .iter()
            .filter(|feature| feature.passes)
            .count()
    }

    pub fn total(&self) -> usize {
        self.features.len()
    }

    pub fn all_pass(&self) -> bool {
        self.passing() == self.total()
    }

    /// Ids of the passing features.
    pub fn passing_ids(&self) -> HashSet<String> {
        self.features
            .iter()
            .filter(|feature| feature.passes)
            .map(|feature| feature.id.clone())
            .collect()
    }

    /// Records that a feature passes and saves the list. The caller has
    /// checked the feature's verify command.
    pub fn mark_passing(&mut self, id: &str) -> Result<()> {
        if self.get(id).is_none() {
            bail!("no feature has the id `{id}`");
        }
        self.note_edits();
        for feature in &mut self.features {
            if feature.id == id {
                feature.passes = true;
            }
        }
        self.write()
    }

    /// Puts the harness's copy back into the file. Returns whether the file
    /// had been changed outside the harness since the harness last wrote it,
    /// in content and not just formatting (a deleted file counts).
    pub fn restore(&mut self) -> Result<bool> {
        self.note_edits();
        self.write()?;
        Ok(std::mem::take(&mut self.edited))
    }

    /// Remembers whether the file says something other than the harness's
    /// copy.
    fn note_edits(&mut self) {
        let same = fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Feature>>(&text).ok())
            .is_some_and(|features| features == self.features);
        self.edited |= !same;
    }

    fn write(&self) -> Result<()> {
        let wanted = to_json(&self.features);
        if fs::read_to_string(&self.path).ok().as_deref() == Some(wanted.as_str()) {
            return Ok(());
        }
        write_atomic(&self.path, wanted.as_bytes())
            .with_context(|| format!("cannot write {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(id: &str, verify: &str, passes: bool) -> Value {
        serde_json::json!({"id": id, "description": format!("does {id}"), "verify": verify, "passes": passes})
    }

    fn parse_value(value: Value) -> Result<Vec<Feature>> {
        parse(&value.to_string())
    }

    #[test]
    fn parses_a_valid_list_and_keeps_extra_fields() {
        let mut first = feature("F1", "true", false);
        first["steps"] = serde_json::json!(["open the page", "click"]);
        let features = parse_value(serde_json::json!([
            first,
            feature("F2", "test -f x", false)
        ]))
        .unwrap();
        assert_eq!(features.len(), 2);
        assert_eq!(features[1].verify, "test -f x");
        check_fresh(&features).unwrap();
        let written: Value = serde_json::from_str(&to_json(&features)).unwrap();
        assert_eq!(written[0]["steps"][1], "click");
    }

    #[test]
    fn rejects_duplicate_ids() {
        let err = parse_value(serde_json::json!([
            feature("F1", "true", false),
            feature("F1", "false", false)
        ]))
        .unwrap_err();
        assert!(
            err.to_string().contains("id `F1` is used more than once"),
            "{err}"
        );
    }

    #[test]
    fn rejects_missing_or_empty_verify() {
        let err = parse_value(serde_json::json!([
            {"id": "F1", "description": "x", "passes": false}
        ]))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("missing field `verify`"),
            "{err:#}"
        );

        let err = parse_value(serde_json::json!([
            feature("F1", "true", false),
            feature("F2", "  ", false)
        ]))
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("`F2`) has an empty verify command"),
            "{err}"
        );
    }

    #[test]
    fn lists_every_problem() {
        let err = parse_value(serde_json::json!([
            feature("", "true", false),
            feature("F2", "", false),
            feature("F2", "true", false)
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("#1 has an empty id"), "{err}");
        assert!(err.contains("empty verify"), "{err}");
        assert!(err.contains("`F2` is used more than once"), "{err}");
    }

    #[test]
    fn rejects_other_shapes() {
        assert!(parse("{}").is_err());
        assert!(parse("[]").unwrap_err().to_string().contains("no features"));
        assert!(
            parse(r#"[{"id": "F1", "description": "x", "verify": "true", "passes": "no"}]"#)
                .is_err()
        );
    }

    #[test]
    fn a_fresh_list_has_nothing_passing() {
        let features = parse_value(serde_json::json!([
            feature("F1", "true", false),
            feature("F2", "true", true)
        ]))
        .unwrap();
        let err = check_fresh(&features).unwrap_err();
        assert!(err.to_string().contains("these are true: F2"), "{err}");
    }

    /// A repository folder whose feature list is `features`.
    fn task_root(features: Value) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(super::super::dir(root.path())).unwrap();
        fs::write(features_path(root.path()), features.to_string()).unwrap();
        root
    }

    #[test]
    fn ledger_rewrites_the_file_in_its_own_format_without_calling_it_an_edit() {
        let root = task_root(serde_json::json!([feature("F1", "true", false)]));
        let mut ledger = Ledger::load(root.path()).unwrap();
        let path = features_path(root.path());
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, to_json(ledger.features()));
        // Same content, other formatting.
        fs::write(
            &path,
            serde_json::json!([feature("F1", "true", false)]).to_string(),
        )
        .unwrap();
        assert!(!ledger.restore().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn ledger_undoes_outside_edits() {
        let root = task_root(serde_json::json!([
            feature("F1", "true", false),
            feature("F2", "true", false)
        ]));
        let mut ledger = Ledger::load(root.path()).unwrap();
        let path = features_path(root.path());
        let original = fs::read_to_string(&path).unwrap();

        fs::write(
            &path,
            serde_json::json!([feature("F1", "true", true)]).to_string(),
        )
        .unwrap();
        assert!(ledger.restore().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(ledger.passing(), 0);
        // Reported once.
        assert!(!ledger.restore().unwrap());

        fs::remove_file(&path).unwrap();
        assert!(ledger.restore().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn mark_passing_saves_and_notices_an_earlier_edit() {
        let root = task_root(serde_json::json!([
            feature("F1", "true", false),
            feature("F2", "true", false)
        ]));
        let mut ledger = Ledger::load(root.path()).unwrap();
        ledger.mark_passing("F1").unwrap();
        assert!(ledger.mark_passing("F9").is_err());
        assert!(load(root.path()).unwrap()[0].passes);
        assert!(!ledger.restore().unwrap());

        let path = features_path(root.path());
        fs::write(&path, "[]").unwrap();
        ledger.mark_passing("F2").unwrap();
        assert!(ledger.all_pass());
        assert!(ledger.restore().unwrap());
        assert_eq!(load(root.path()).unwrap().len(), 2);
    }
}
