//! RIP is a library that allows the resolving and installing of Python PyPI packages from Rust into a virtual environment.
//! It's based on our experience with building Rattler and aims to provide the same experience but for PyPI instead of Conda.
//! It should be fast and easy to use.
//! Like Rattler, this library is not a package manager itself but provides the low-level plumbing to be used in one.

#![deny(missing_docs)]
mod artifact;
mod artifact_name;
mod core_metadata;
mod extra;
mod file_store;
pub mod html;
mod http;
mod package_database;
mod package_name;
mod project_info;
mod reqparse;
mod requirement;
mod rfc822ish;
mod seek_slice;
mod specifier;
mod utils;

#[cfg(feature = "resolvo")]
mod resolve;

#[cfg(feature = "resolvo")]
pub use resolve::resolve;

pub use file_store::{CacheKey, FileStore};
pub use package_database::PackageDb;

pub use artifact::{Artifact, MetadataArtifact, Wheel};
pub use artifact_name::{
    ArtifactName, BuildTag, InnerAsArtifactName, ParseArtifactNameError, SDistFormat, SDistName,
    WheelName,
};
pub use extra::Extra;
pub use package_name::{NormalizedPackageName, PackageName, ParsePackageNameError};
pub use pep440::Version;
pub use project_info::{ArtifactHashes, ArtifactInfo, DistInfoMetadata, Meta, ProjectInfo, Yanked};
pub use requirement::{
    marker, PackageRequirement, ParseExtraInEnv, PythonRequirement, Requirement, UserRequirement,
};
pub use specifier::{CompareOp, Specifier, Specifiers};

pub use utils::normalize_index_url;
