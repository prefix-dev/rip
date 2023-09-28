// Implementation comes from https://github.com/njsmith/posy/blob/main/src/vocab/extra.rs
// Licensed under MIT or Apache-2.0

// 'Extra' string format is not well specified. It looks like what poetry does is simply normalize
// the name and be done with it.
//
// PEP 508's grammar for requirement specifiers says that extras have to
// be "identifiers", which means: first char [A-Za-z0-9], remaining chars also
// allowed to include '-_.'. But in practice we've seen Extras like "ssl:sys_platform=='win32'"
// which do not follow that rule at all.

// ORIGINAL comment from Posy.

// 'Extra' string format is not well specified. It looks like what pip does is
// run things through pkg_resources.safe_extra, which does:
//
//   re.sub('[^A-Za-z0-9.-]+', '_', extra).lower()
//
// So A-Z becomes a-z, a-z 0-9 . - are preserved, and any contiguous run of
// other characters becomes a single _.
//
// OTOH, PEP 508's grammar for requirement specifiers says that extras have to
// be "identifiers", which means: first char [A-Za-z0-9], remaining chars also
// allowed to include -_.
//
// I guess for now I'll just pretend that they act the same as package names,
// and see how long I can get away with it.
//
// There's probably a better way to factor this and reduce code duplication...

use miette::Diagnostic;
use serde::{Serialize, Serializer};
use serde_with::DeserializeFromStr;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, Eq, DeserializeFromStr)]
/// Structure that holds both the source string and the normalized version of an extra.
pub struct Extra {
    /// The original string this instance was created from
    source: Box<str>,

    /// The normalized version of `source`.
    normalized: Box<str>,
}

impl Extra {
    /// Returns the source representation of the name. This is the string from which this
    /// instance was created.
    pub fn as_source_str(&self) -> &str {
        self.source.as_ref()
    }

    /// Returns the normalized version of the name. The normalized string is guaranteed to
    /// be a valid python package name.
    pub fn as_str(&self) -> &str {
        self.normalized.as_ref()
    }
}

#[derive(Debug, Clone, Error, Diagnostic)]
pub enum ParseExtraError {}

impl FromStr for Extra {
    type Err = ParseExtraError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // https://www.python.org/dev/peps/pep-0503/#normalized-names
        let mut normalized = s.replace(['-', '_', '.'], "-");
        normalized.make_ascii_lowercase();

        Ok(Self {
            source: s.to_owned().into_boxed_str(),
            normalized: normalized.into_boxed_str(),
        })
    }
}

impl Hash for Extra {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.normalized.hash(state)
    }
}

impl PartialEq for Extra {
    fn eq(&self, other: &Self) -> bool {
        self.normalized.eq(&other.normalized)
    }
}

impl PartialOrd for Extra {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Extra {
    fn cmp(&self, other: &Self) -> Ordering {
        self.normalized.cmp(&other.normalized)
    }
}

impl Serialize for Extra {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.source.as_ref().serialize(serializer)
    }
}
