// Implementation comes from https://github.com/njsmith/posy/blob/main/src/vocab/core_metadata.rs
// Licensed under MIT or Apache-2.0

use crate::{
    extra::Extra, package_name::PackageName, rfc822ish::RFC822ish, Version, VersionSpecifiers,
};
use miette::IntoDiagnostic;
use once_cell::sync::Lazy;
use pep508_rs::Requirement;
use std::{collections::HashSet, str::FromStr};

#[derive(Debug, Clone)]
pub struct WheelCoreMetadata {
    pub name: PackageName,
    pub version: Version,
    pub requires_dist: Vec<Requirement>,
    pub requires_python: Option<VersionSpecifiers>,
    pub extras: HashSet<Extra>,
}

impl TryFrom<&[u8]> for WheelCoreMetadata {
    type Error = miette::Report;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        let (name, version, mut parsed) = parse_common(value)?;

        let mut requires_dist = Vec::new();
        for req_str in parsed.take_all("Requires-Dist").into_iter() {
            match req_str.parse() {
                Err(e) => {
                    tracing::error!("ignoring Requires-Dist: {req_str}, failed to parse: {e}")
                }
                Ok(req) => requires_dist.push(req),
            }
        }

        let requires_python = parsed
            .maybe_take("Requires-Python")?
            .as_deref()
            .map(VersionSpecifiers::from_str)
            .transpose()
            .into_diagnostic()?;

        let mut extras: HashSet<Extra> = HashSet::new();
        for extra in parsed.take_all("Provides-Extra").drain(..) {
            extras.insert(extra.parse()?);
        }

        Ok(WheelCoreMetadata {
            name,
            version,
            requires_dist,
            requires_python,
            extras,
        })
    }
}

fn parse_common(input: &[u8]) -> miette::Result<(PackageName, Version, RFC822ish)> {
    let input = String::from_utf8_lossy(input);
    let mut parsed = RFC822ish::from_str(&input).into_diagnostic()?;

    static NEXT_MAJOR_METADATA_VERSION: Lazy<Version> =
        Lazy::new(|| Version::from_str("3").unwrap());

    // Quoth https://packaging.python.org/specifications/core-metadata:
    // "Automated tools consuming metadata SHOULD warn if metadata_version
    // is greater than the highest version they support, and MUST fail if
    // metadata_version has a greater major version than the highest
    // version they support (as described in PEP 440, the major version is
    // the value before the first dot)."
    //
    // We do the MUST, but I think I disagree about warning on
    // unrecognized minor revisions. If it's a minor revision, then by
    // definition old software is supposed to be able to handle it "well
    // enough". The only purpose of the warning would be to alert users
    // that they might want to upgrade, or to alert the tool authors that
    // there's a new metadata release. But for users, there are better
    // ways to nudge them to upgrade (e.g. checking on startup, like
    // pip does), and new metadata releases are so rare and so
    // much-discussed beforehand that if a tool's authors don't know
    // about it it's because the tool is abandoned anyway.
    let metadata_version = parsed.take("Metadata-Version")?;
    let metadata_version: Version = metadata_version
        .parse()
        .map_err(|err| miette::miette!("failed to parse {metadata_version}: {err}"))?;
    if metadata_version >= *NEXT_MAJOR_METADATA_VERSION {
        miette::bail!("unsupported Metadata-Version {}", metadata_version);
    }

    let version_str = parsed.take("Version")?;

    Ok((
        parsed.take("Name")?.parse()?,
        version_str
            .parse()
            .map_err(|err| miette::miette!("failed to parse version '{version_str}': {err}"))?,
        parsed,
    ))
}
