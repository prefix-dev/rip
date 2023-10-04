//! Structs that represent the response from the Simple API when using JSON (PEP 691).

use crate::artifact::Artifact;
use crate::artifact_name::ArtifactName;
use pep440_rs::VersionSpecifiers;
use rattler_digest::{serde::SerializableHash, Sha256};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, skip_serializing_none, DisplayFromStr, VecSkipError};

/// Represents the result of the response from the Simple API.
#[serde_as]
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectInfo {
    /// Metadata describing the API.
    pub meta: Meta,

    /// All the available files for this project
    #[serde_as(as = "VecSkipError<_>")]
    pub files: Vec<ArtifactInfo>,
}

/// Describes a single artifact that is available for download.
#[serde_as]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ArtifactInfo {
    /// Artifact name
    pub filename: ArtifactName,
    /// Url to download the artifact
    pub url: url::Url,
    /// Hashes of the artifact
    pub hashes: Option<ArtifactHashes>,
    /// Python requirement
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub requires_python: Option<VersionSpecifiers>,
    #[serde(default)]
    /// This attribute specified if the metadata is available
    /// as a separate download described in [PEP 658](https://www.python.org/dev/peps/pep-0658/)
    pub dist_info_metadata: DistInfoMetadata,
    /// Yanked information
    #[serde(default)]
    pub yanked: Yanked,
}

impl ArtifactInfo {
    /// Returns true if this artifact describes an instance of `T`.
    pub fn is<T: Artifact>(&self) -> bool {
        self.filename.as_inner::<T::Name>().is_some()
    }
}

/// Describes a set of hashes for a certain artifact. In theory all hash algorithms available via
/// Pythons `hashlib` are supported but we only support some common ones.
#[serde_as]
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ArtifactHashes {
    #[serde_as(as = "Option<SerializableHash<Sha256>>")]
    /// Contains the optional sha256 hash of the artifact
    pub sha256: Option<rattler_digest::Sha256Hash>,
}

impl ArtifactHashes {
    /// Returns true if this instance does not contain a single hash.
    pub fn is_empty(&self) -> bool {
        self.sha256.is_none()
    }
}

/// Describes whether the metadata is available for download from the index as specified in PEP 658
/// (`{file_url}.metadata`). An index might also include hashes of the metadata file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(from = "Option<RawDistInfoMetadata>")]
pub struct DistInfoMetadata {
    /// True if the metadata is available
    pub available: bool,
    /// Hashes to verify the metadata file
    pub hashes: ArtifactHashes,
}

/// An optional key that indicates that metadata for this file is available, via the same location
/// as specified in PEP 658 ({file_url}.metadata). Where this is present, it MUST be either a
/// boolean to indicate if the file has an associated metadata file, or a dictionary mapping hash
/// names to a hex encoded digest of the metadata’s hash.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawDistInfoMetadata {
    NoHashes(bool),
    WithHashes(ArtifactHashes),
}

impl From<Option<RawDistInfoMetadata>> for DistInfoMetadata {
    fn from(maybe_raw: Option<RawDistInfoMetadata>) -> Self {
        match maybe_raw {
            None => Default::default(),
            Some(raw) => match raw {
                RawDistInfoMetadata::NoHashes(available) => Self {
                    available,
                    hashes: Default::default(),
                },
                RawDistInfoMetadata::WithHashes(hashes) => Self {
                    available: true,
                    hashes,
                },
            },
        }
    }
}

/// Meta information stored in the [`ProjectInfo`]. It represents the version of the API. Clients
/// should verify that the contents is as expected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Meta {
    #[serde(rename = "api-version")]
    /// Version of the API
    pub version: String,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            version: "1.0".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawYanked {
    NoReason(bool),
    WithReason(String),
}

/// Struct that describes whether a package is yanked or not.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(from = "RawYanked")]
pub struct Yanked {
    /// This is true if the package is yanked.
    pub yanked: bool,
    /// Optional reason why the package is yanked.
    pub reason: Option<String>,
}

impl From<RawYanked> for Yanked {
    fn from(raw: RawYanked) -> Self {
        match raw {
            RawYanked::NoReason(yanked) => Self {
                yanked,
                reason: None,
            },
            RawYanked::WithReason(reason) => Self {
                yanked: true,
                reason: Some(reason),
            },
        }
    }
}
