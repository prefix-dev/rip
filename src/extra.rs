// Implementation comes from https://github.com/njsmith/posy/blob/main/src/vocab/extra.rs
// Licensed under MIT or Apache-2.0

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

use std::borrow::Borrow;
use std::ops::Deref;
use crate::package_name::{PackageName, ParsePackageNameError};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize, Serialize, Hash, PartialEq, Eq)]
pub struct Extra(PackageName);

impl Extra {
    pub fn as_source_str(&self) -> &str {
        self.0.as_source_str()
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl FromStr for Extra {
    type Err = ParsePackageNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Extra)
    }
}

impl AsRef<PackageName> for Extra {
    fn as_ref(&self) -> &PackageName {
        &self.0
    }
}

impl Borrow<PackageName> for Extra {
    fn borrow(&self) -> &PackageName {
        &self.0
    }
}

impl Deref for Extra {
    type Target = PackageName;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
