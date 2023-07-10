use regex::Regex;
use serde::{Serialize, Serializer};
use serde_with::DeserializeFromStr;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::OnceLock;
use thiserror::Error;

/// A representation of a python package name. This struct both stores the source string from which
/// this instance was created as well as a normalized name that can be used to compare different
/// names. The normalized name is guaranteed to be a valid python package name.
#[derive(Debug, Clone, Eq, DeserializeFromStr)]
pub struct PackageName {
    /// The original string this instance was created from
    source: Box<str>,

    /// The normalized version of `source`.
    normalized: Box<str>,
}

impl PackageName {
    /// Returns the source representation of the package name. This is the string from which this
    /// instance was created.
    pub fn as_source_str(&self) -> &str {
        self.source.as_ref()
    }

    /// Returns the normalized version of the package name. The normalized string is guaranteed to
    /// be a valid python package name.
    pub fn as_str(&self) -> &str {
        self.normalized.as_ref()
    }
}

#[derive(Debug, Clone, Error)]
pub enum ParsePackageNameError {
    #[error("invalid package name '{0}'")]
    InvalidPackageName(String),
}

impl FromStr for PackageName {
    type Err = ParsePackageNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        static NAME_VALIDATE: OnceLock<Regex> = OnceLock::new();
        let name_validate = NAME_VALIDATE.get_or_init(|| {
            // https://packaging.python.org/specifications/core-metadata/#name
            Regex::new(r"(?i-u)^([A-Z0-9]|[A-Z0-9][A-Z0-9._-]*[A-Z0-9])$").unwrap()
        });

        if !name_validate.is_match(s) {
            return Err(ParsePackageNameError::InvalidPackageName(s.into()));
        }

        // https://www.python.org/dev/peps/pep-0503/#normalized-names
        let mut normalized = s.replace(&['-', '_', '.'], "-");
        normalized.make_ascii_lowercase();

        Ok(PackageName {
            source: s.to_owned().into_boxed_str(),
            normalized: normalized.into_boxed_str(),
        })
    }
}

impl Hash for PackageName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.normalized.hash(state)
    }
}

impl PartialEq for PackageName {
    fn eq(&self, other: &Self) -> bool {
        self.normalized.eq(&other.normalized)
    }
}

impl PartialOrd for PackageName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PackageName {
    fn cmp(&self, other: &Self) -> Ordering {
        self.normalized.cmp(&other.normalized)
    }
}

impl Serialize for PackageName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.source.as_ref().serialize(serializer)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_packagename_basics() {
        let name1: PackageName = "Foo-Bar-Baz".parse().unwrap();
        assert_eq!(name1.as_source_str(), "Foo-Bar-Baz");
        assert_eq!(name1.as_str(), "foo-bar-baz");

        let name2: PackageName = "foo_bar.baz".parse().unwrap();
        assert_eq!(name2.as_source_str(), "foo_bar.baz");
        assert_eq!(name2.as_str(), "foo-bar-baz");

        assert_eq!(name1, name2);

        let name3: PackageName = "foo-barbaz".parse().unwrap();
        assert_ne!(name1, name3);
    }
}
