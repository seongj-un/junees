//! Level 1 of a draft: the plan, a JSON object of the whole text's
//! sections and the concept of each, read from the model's reply.

use serde::Serialize;
use serde_json::Value;

use super::{MAX_SECTIONS, MIN_SECTIONS};

/// Characters of a section concept that are kept; the rest is cut.
pub const CONCEPT_CHARS: usize = 300;

/// Characters of the title that are kept.
pub const TITLE_CHARS: usize = 200;

/// Characters of a section id the model may choose.
const ID_CHARS: usize = 32;

/// The plan of a draft: its title and sections, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub title: String,
    pub sections: Vec<PlanSection>,
}

/// One section of a [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanSection {
    pub id: String,
    pub concept: String,
}

impl Plan {
    /// The plan as the later levels' prompts show it: compact JSON, the
    /// same bytes for every request of a level.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a plan always serializes")
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.sections.iter().position(|section| section.id == id)
    }
}

/// Reads the plan from the model's reply: one JSON object, maybe inside a
/// code fence or with words around it. Fewer than [`MIN_SECTIONS`]
/// sections or broken JSON is an error (the caller asks once more); more
/// than [`MAX_SECTIONS`] are cut to that many. Concepts become one line,
/// and ids that are missing, repeated or not plain (`[A-Za-z0-9_-]`, at
/// most 32 characters) are replaced by `s<n>`, so later prompts can name
/// them safely.
pub fn parse(reply: &str) -> Result<Plan, String> {
    let json = object_text(reply).ok_or("the reply holds no JSON object")?;
    let value: Value =
        serde_json::from_str(json).map_err(|err| format!("the JSON does not parse: {err}"))?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(one_line)
        .filter(|title| !title.is_empty())
        .ok_or("`title` is missing or empty")?;
    let sections = value
        .get("sections")
        .and_then(Value::as_array)
        .ok_or("`sections` is missing or not a list")?;
    let mut parsed: Vec<PlanSection> = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        let concept = section
            .get("concept")
            .and_then(Value::as_str)
            .map(one_line)
            .filter(|concept| !concept.is_empty())
            .ok_or_else(|| format!("section {} has no `concept`", index + 1))?;
        let id = section
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        parsed.push(PlanSection {
            id,
            concept: cut(&concept, CONCEPT_CHARS),
        });
    }
    if parsed.len() < MIN_SECTIONS {
        return Err(format!(
            "the plan has {} section(s); it needs {MIN_SECTIONS} to {MAX_SECTIONS}",
            parsed.len()
        ));
    }
    parsed.truncate(MAX_SECTIONS);
    // Keep the first of each plain id, then give the rest the first free
    // `s<n>`.
    let mut seen: Vec<String> = Vec::new();
    for section in parsed.iter_mut() {
        if plain_id(&section.id) && !seen.contains(&section.id) {
            seen.push(section.id.clone());
        } else {
            section.id.clear();
        }
    }
    let mut next = 1;
    for section in parsed.iter_mut().filter(|section| section.id.is_empty()) {
        let id = loop {
            let id = format!("s{next}");
            next += 1;
            if !seen.contains(&id) {
                break id;
            }
        };
        seen.push(id.clone());
        section.id = id;
    }
    Ok(Plan {
        title: cut(&title, TITLE_CHARS),
        sections: parsed,
    })
}

fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= ID_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The text from the first `{` to its matching `}`, skipping braces inside
/// JSON strings.
pub(super) fn object_text(reply: &str) -> Option<&str> {
    let start = reply.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, c) in reply[start..].char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&reply[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whitespace runs (newlines included) as single spaces, trimmed.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first `max` characters of `text`.
pub(super) fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}
