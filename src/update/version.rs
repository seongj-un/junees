//! Release versions: `MAJOR.MINOR.PATCH` with an optional pre-release
//! (`1.2.0-rc.1`), ordered by semver precedence.

use std::cmp::Ordering;
use std::fmt;

/// A release version. A tag is the version with a `v` in front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// The pre-release identifiers (`rc`, `1` of `-rc.1`); empty for a
    /// release.
    pre: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Number(u64),
    Text(String),
}

impl Version {
    /// Reads `1.2.3`, `v1.2.3` or `1.2.3-rc.1`. Build metadata (`+...`)
    /// is not taken: release tags have none.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "`{}` is not a release version (like 0.2.0 or v0.2.0)",
                crate::prompt_text::escape_hidden(text)
            )
        };
        let bare = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match bare.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (bare, None),
        };
        let numbers: Vec<u64> = core
            .split('.')
            .map(number)
            .collect::<Option<_>>()
            .ok_or_else(bad)?;
        let [major, minor, patch] = numbers[..] else {
            return Err(bad());
        };
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|ident| {
                    if ident.is_empty()
                        || !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    {
                        None
                    } else if ident.chars().all(|c| c.is_ascii_digit()) {
                        number(ident).map(Ident::Number)
                    } else {
                        Some(Ident::Text(ident.to_string()))
                    }
                })
                .collect::<Option<_>>()
                .ok_or_else(bad)?,
        };
        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// The git tag of this version: `v1.2.3`.
    pub fn tag(&self) -> String {
        format!("v{self}")
    }

    /// Whether this is a pre-release (`-rc.1`).
    pub fn is_pre_release(&self) -> bool {
        !self.pre.is_empty()
    }
}

/// A number without a leading zero (`0` itself is fine).
fn number(text: &str) -> Option<u64> {
    if text.is_empty()
        || !text.chars().all(|c| c.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, ident) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match ident {
                Ident::Number(n) => write!(f, "{n}")?,
                Ident::Text(text) => f.write_str(text)?,
            }
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                // A release comes after its pre-releases.
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Ident::Number(a), Ident::Number(b)) => a.cmp(b),
            // Numeric identifiers come before alphanumeric ones.
            (Ident::Number(_), Ident::Text(_)) => Ordering::Less,
            (Ident::Text(_), Ident::Number(_)) => Ordering::Greater,
            (Ident::Text(a), Ident::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn reads_versions_and_tags() {
        assert_eq!(v("0.1.0").to_string(), "0.1.0");
        assert_eq!(v("v0.1.0"), v("0.1.0"));
        assert_eq!(v("v1.20.3").tag(), "v1.20.3");
        assert_eq!(v("1.0.0-rc.1").to_string(), "1.0.0-rc.1");
        assert!(v("1.0.0-rc.1").is_pre_release());
        assert!(!v("1.0.0").is_pre_release());
        assert_eq!(
            v(env!("CARGO_PKG_VERSION")).to_string(),
            env!("CARGO_PKG_VERSION")
        );
        for bad in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.x",
            "1.2.3-",
            "1.2.3-rc..1",
            "1.2.3-01",
            "1.2.3+build",
            "vv1.2.3",
            " 1.2.3",
            "1.2.3-rc/1",
            "-1.2.3",
        ] {
            let err = Version::parse(bad).unwrap_err();
            assert!(err.contains("is not a release version"), "{bad}: {err}");
        }
    }

    #[test]
    fn orders_by_semver_precedence() {
        let ordered = [
            "0.1.0",
            "0.1.1",
            "0.2.0-alpha",
            "0.2.0-alpha.1",
            "0.2.0-alpha.beta",
            "0.2.0-beta",
            "0.2.0-beta.2",
            "0.2.0-beta.11",
            "0.2.0-rc.1",
            "0.2.0",
            "0.10.0",
            "1.0.0",
            "10.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
            assert!(v(pair[1]) > v(pair[0]), "{} > {}", pair[1], pair[0]);
        }
        assert_eq!(v("v2.0.0").cmp(&v("2.0.0")), Ordering::Equal);
    }
}
