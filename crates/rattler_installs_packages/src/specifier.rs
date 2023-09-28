// Implementation comes from https://github.com/njsmith/posy/blob/main/src/vocab/specifier.rs
// Licensed under MIT or Apache-2.0

use miette::{Context, IntoDiagnostic};
use once_cell::sync::Lazy;
use pep440::Version;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use smallvec::{smallvec, SmallVec};
use std::{fmt::Display, ops::Range, str::FromStr};

// TODO: See if we can parse this a little better than just an operator and a string. Every time
//  `satisfied_by` is called `to_ranges` is called. We can probably cache that.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// A specifier is a comparison operator and a version.
/// See [PEP-440](https://peps.python.org/pep-0440/#version-specifiers)
pub struct Specifier {
    /// Compartions operator
    pub op: CompareOp,
    /// Version
    pub value: String,
}

impl Specifier {
    /// Returns true if the specifier is satisfied by the given version.
    pub fn satisfied_by(&self, version: &Version) -> miette::Result<bool> {
        Ok(self.to_ranges()?.into_iter().any(|r| r.contains(version)))
    }

    /// Converts the specifier to a set of ranges.
    pub fn to_ranges(&self) -> miette::Result<SmallVec<[Range<Version>; 1]>> {
        self.op.ranges(&self.value)
    }
}

impl Display for Specifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.op, self.value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SerializeDisplay, DeserializeFromStr, Default, Hash)]
/// A collection of specifiers, separated by commas.
pub struct Specifiers(pub Vec<Specifier>);

impl Specifiers {
    /// Returns true if the set of specifiers is satisfied by the given version.
    pub fn satisfied_by(&self, version: &Version) -> miette::Result<bool> {
        for specifier in &self.0 {
            if !specifier.satisfied_by(version)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl Display for Specifiers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for spec in &self.0 {
            if !first {
                write!(f, ", ")?
            }
            first = false;
            write!(f, "{}", spec)?
        }
        Ok(())
    }
}

impl FromStr for Specifiers {
    type Err = miette::Report;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let specifiers_or_err = super::reqparse::versionspec(input);
        specifiers_or_err
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to parse versions specifiers from {:?}", input))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
/// Models a comparison operator in a version specifier.
pub enum CompareOp {
    LessThanEqual,
    StrictlyLessThan,
    NotEqual,
    Equal,
    GreaterThanEqual,
    StrictlyGreaterThan,
    Compatible,
}

impl Display for CompareOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use CompareOp::*;
        write!(
            f,
            "{}",
            match self {
                LessThanEqual => "<=",
                StrictlyLessThan => "<",
                NotEqual => "!=",
                Equal => "==",
                GreaterThanEqual => ">=",
                StrictlyGreaterThan => ">",
                Compatible => "~=",
            }
        )
    }
}

impl FromStr for CompareOp {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        use CompareOp::*;
        Ok(match value {
            "==" => Equal,
            "!=" => NotEqual,
            "<=" => LessThanEqual,
            "<" => StrictlyLessThan,
            ">=" => GreaterThanEqual,
            ">" => StrictlyGreaterThan,
            "~=" => Compatible,
            "===" => miette::bail!("'===' is not implemented"),
            _ => miette::bail!("unrecognized operator: {:?}", value),
        })
    }
}

fn parse_version_wildcard(input: &str) -> miette::Result<(Version, bool)> {
    let (vstr, wildcard) = if let Some(vstr) = input.strip_suffix(".*") {
        (vstr, true)
    } else {
        (input, false)
    };
    let version: Version =
        Version::parse(vstr).ok_or_else(|| miette::miette!("failed to parse version '{vstr}'"))?;
    Ok((version, wildcard))
}

impl CompareOp {
    /// Converts a comparison like ">= 1.2" into a union of [half, open) ranges.
    ///
    /// Has to take a string, not a Version, because == and != can take "wildcards", which
    /// are not valid versions.
    pub fn ranges(&self, rhs: &str) -> miette::Result<SmallVec<[Range<Version>; 1]>> {
        use CompareOp::*;
        let (version, wildcard) = parse_version_wildcard(rhs)?;
        Ok(if wildcard {
            if version.dev.is_some() || !version.local.is_empty() {
                miette::bail!("version wildcards can't have dev or local suffixes");
            }
            // == X.* corresponds to the half-open range
            //
            // [X.dev0, (X+1).dev0)
            let mut low = version.clone();
            low.dev = Some(0);
            let mut high = version;
            // .* can actually appear after .postX or .aX, so we need to find the last
            // numeric entry in the version, and increment that.
            if let Some(post) = high.post {
                high.post = Some(post + 1)
            } else if let Some(pre) = high.pre {
                use pep440::PreRelease::*;
                high.pre = Some(match pre {
                    RC(n) => RC(n + 1),
                    A(n) => A(n + 1),
                    B(n) => B(n + 1),
                })
            } else {
                *high.release.last_mut().unwrap() += 1;
            }
            high.dev = Some(0);
            match self {
                Equal => smallvec![low..high],
                NotEqual => {
                    smallvec![VERSION_ZERO.clone()..low, high..VERSION_INFINITY.clone()]
                }
                _ => miette::bail!("Can't use wildcard with {:?}", self),
            }
        } else {
            // no wildcards here
            if self != &Equal && self != &NotEqual && !version.local.is_empty() {
                miette::bail!(
                    "Operator {:?} cannot be used on a version with a +local suffix",
                    self
                );
            }
            match self {
                // These two are simple
                LessThanEqual => smallvec![VERSION_ZERO.clone()..version.next()],
                GreaterThanEqual => smallvec![version..VERSION_INFINITY.clone()],
                // These are also pretty simple, because we took care of the wildcard
                // cases up above.
                Equal => smallvec![version.clone()..version.next()],
                NotEqual => smallvec![
                    VERSION_ZERO.clone()..version.clone(),
                    version.next()..VERSION_INFINITY.clone(),
                ],
                // "The exclusive ordered comparison >V MUST NOT allow a post-release of
                // the given version unless V itself is a post release."
                StrictlyGreaterThan => {
                    let mut low = version.clone();
                    if let Some(dev) = &version.dev {
                        low.dev = Some(dev + 1);
                    } else if let Some(post) = &version.post {
                        low.post = Some(post + 1);
                    } else {
                        // Otherwise, want to increment either the pre-release (a0 ->
                        // a1), or the "last" release segment. But working with
                        // pre-releases takes a lot of typing, and there is no "last"
                        // release segment -- X.Y.Z is just shorthand for
                        // X.Y.Z.0.0.0.0... So instead, we tack on a .post(INFINITY) and
                        // hope no-one actually makes a version like this in practice.
                        low.post = Some(u32::MAX);
                    }
                    smallvec![low..VERSION_INFINITY.clone()]
                }
                // "The exclusive ordered comparison <V MUST NOT allow a pre-release of
                // the specified version unless the specified version is itself a
                // pre-release."
                StrictlyLessThan => {
                    if (&version.pre, &version.dev) == (&None, &None) {
                        let mut new_max = version;
                        new_max.dev = Some(0);
                        new_max.post = None;
                        new_max.local = vec![];
                        smallvec![VERSION_ZERO.clone()..new_max]
                    } else {
                        // Otherwise, some kind of pre-release
                        smallvec![VERSION_ZERO.clone()..version]
                    }
                }
                // ~= X.Y.suffixes is the same as >= X.Y.suffixes && == X.*
                // So it's a half-open range:
                //   [X.Y.suffixes, (X+1).dev0)
                Compatible => {
                    if version.release.len() < 2 {
                        miette::bail!("~= operator requires a version with two segments (X.Y)");
                    }
                    let mut new_max = pep440::Version {
                        epoch: version.epoch,
                        release: version.release.clone(),
                        pre: None,
                        post: None,
                        dev: Some(0),
                        local: vec![],
                    };
                    // Unwraps here are safe because we confirmed that the vector has at
                    // least 2 elements above.
                    new_max.release.pop().unwrap();
                    *new_max.release.last_mut().unwrap() += 1;
                    smallvec![version..new_max]
                }
            }
        })
    }
}

pub static VERSION_ZERO: Lazy<Version> = Lazy::new(|| Version::parse("0a0.dev0").unwrap());

pub static VERSION_INFINITY: Lazy<Version> = Lazy::new(|| {
    // Technically there is no largest PEP 440 version. But this should be good
    // enough that no-one will notice the difference...
    pep440::Version {
        epoch: u32::MAX,
        release: vec![u32::MAX, u32::MAX, u32::MAX],
        pre: None,
        post: Some(u32::MAX),
        dev: None,
        local: vec![],
    }
});

trait VersionExt {
    fn next(&self) -> Self;
}

impl VersionExt for Version {
    /// Returns the smallest PEP 440 version that is larger than self.
    fn next(&self) -> Version {
        let mut new = self.clone();
        // The rules are here:
        //
        //   https://www.python.org/dev/peps/pep-0440/#summary-of-permitted-suffixes-and-relative-ordering
        //
        // The relevant ones for this:
        //
        // - You can't attach a .postN after a .devN. So if you have a .devN,
        //   then the next possible version is .dev(N+1)
        //
        // - You can't attach a .postN after a .postN. So if you already have
        //   a .postN, then the next possible value is .post(N+1).
        //
        // - You *can* attach a .postN after anything else. And a .devN after that. So
        // to get the next possible value, attach a .post0.dev0.
        if let Some(dev) = &mut new.dev {
            *dev += 1;
        } else if let Some(post) = &mut new.post {
            *post += 1;
        } else {
            new.post = Some(0);
            new.dev = Some(0);
        }
        new
    }
}
