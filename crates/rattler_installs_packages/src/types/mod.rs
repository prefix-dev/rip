//! This module contains all the types for working with PyPA packaging repositories.
//! We have tried to follow the PEP's and PyPA packaging guide as closely as possible.
mod artifact;

mod artifact_name;

mod package_name;

mod core_metadata;

mod record;

mod extra;

mod entry_points;

mod project_info;

mod direct_url_json;
mod rfc822ish;

pub use artifact::Artifact;

pub use artifact_name::{
    ArtifactName, BuildTag, InnerAsArtifactName, ParseArtifactNameError, SDistFilename,
    SDistFormat, WheelFilename,
};

pub use direct_url_json::{DirectUrlHashes, DirectUrlJson, DirectUrlSource, DirectUrlVcs};

pub use core_metadata::{MetadataVersion, WheelCoreMetaDataError, WheelCoreMetadata};

pub use record::{Record, RecordEntry};

pub use package_name::{NormalizedPackageName, PackageName, ParsePackageNameError};

pub use extra::Extra;

pub use entry_points::{EntryPoint, ParseEntryPointError};

pub use project_info::{ArtifactHashes, ArtifactInfo, DistInfoMetadata, Meta, ProjectInfo, Yanked};

pub(crate) use rfc822ish::RFC822ish;

pub use pep440_rs::{Version, VersionSpecifier, VersionSpecifiers};
pub use pep508_rs::{MarkerEnvironment, Requirement};
