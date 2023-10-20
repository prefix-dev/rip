// Implementation comes from https://github.com/njsmith/posy/blob/main/src/vocab/core_metadata.rs
// Licensed under MIT or Apache-2.0

use crate::extra::ParseExtraError;
use crate::{
    extra::Extra, package_name::PackageName, rfc822ish::RFC822ish, ParsePackageNameError, Version,
    VersionSpecifiers,
};
use once_cell::sync::Lazy;
use pep440_rs::Pep440Error;
use pep508_rs::Requirement;
use std::{collections::HashSet, str::FromStr};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct WheelCoreMetadata {
    pub name: PackageName,
    pub version: Version,
    pub requires_dist: Vec<Requirement>,
    pub requires_python: Option<VersionSpecifiers>,
    pub extras: HashSet<Extra>,
}

#[derive(Debug, Error)]
pub enum WheelCoreMetaDataError {
    #[error(transparent)]
    FailedToParseMetadata(#[from] <RFC822ish as FromStr>::Err),

    #[error("missing key {0} in METADATA")]
    MissingKey(String),

    #[error("duplicate key {0} in METADATA")]
    DuplicateKey(String),

    #[error("invalid Metadata-Version: {0}")]
    InvalidMetadataVersion(String),

    #[error("invalid Version: {0}")]
    InvalidVersion(String),

    #[error("invalid Requires-Python: {0}")]
    InvalidRequiresPython(#[source] Pep440Error),

    #[error("unsupported METADATA version {0}")]
    UnsupportedVersion(Version),

    #[error(transparent)]
    InvalidPackageName(#[from] ParsePackageNameError),

    #[error("invalid extra identifier '{0}'")]
    InvalidExtra(String, #[source] ParseExtraError),

    #[error("{0}")]
    FailedToParse(String),
}

impl TryFrom<&[u8]> for WheelCoreMetadata {
    type Error = WheelCoreMetaDataError;

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
            .maybe_take("Requires-Python")
            .map_err(|_| WheelCoreMetaDataError::DuplicateKey(String::from("Requires-Python")))?
            .as_deref()
            .map(VersionSpecifiers::from_str)
            .transpose()
            .map_err(WheelCoreMetaDataError::InvalidRequiresPython)?;

        let mut extras: HashSet<Extra> = HashSet::new();
        for extra in parsed.take_all("Provides-Extra").drain(..) {
            extras.insert(
                extra
                    .parse()
                    .map_err(|e| WheelCoreMetaDataError::InvalidExtra(extra, e))?,
            );
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

fn parse_common(input: &[u8]) -> Result<(PackageName, Version, RFC822ish), WheelCoreMetaDataError> {
    let input = String::from_utf8_lossy(input);
    let mut parsed = RFC822ish::from_str(&input)?;

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
    let metadata_version = parsed
        .take("Metadata-Version")
        .map_err(|_| WheelCoreMetaDataError::MissingKey(String::from("Metadata-Version")))?;
    let metadata_version: Version = metadata_version
        .parse()
        .map_err(WheelCoreMetaDataError::InvalidMetadataVersion)?;
    if metadata_version >= *NEXT_MAJOR_METADATA_VERSION {
        return Err(WheelCoreMetaDataError::UnsupportedVersion(metadata_version));
    }

    let version_str = parsed
        .take("Version")
        .map_err(|_| WheelCoreMetaDataError::MissingKey(String::from("Version")))?;

    Ok((
        parsed
            .take("Name")
            .map_err(|_| WheelCoreMetaDataError::MissingKey(String::from("Name")))?
            .parse()?,
        version_str
            .parse()
            .map_err(WheelCoreMetaDataError::InvalidVersion)?,
        parsed,
    ))
}
