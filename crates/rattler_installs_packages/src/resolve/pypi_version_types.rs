//! This module contains types that are used to represent versions and version sets
//! these are used by the [`resolvo`] crate to resolve dependencies.
//! This module, in combination with the [`super::dependency_provider`] modules is used to make the PyPI ecosystem compatible with the [`resolvo`] crate.

use crate::resolve::solve_options::PreReleaseResolution;
use crate::types::{Extra, NormalizedPackageName};
use pep440_rs::Version;
use pep508_rs::VersionOrUrl;
use resolvo::utils::VersionSet;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use url::Url;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
/// This is a wrapper around Specifiers that implements [`VersionSet`]
pub struct PypiVersionSet {
    /// The spec to match against
    spec: Option<VersionOrUrl>,
    /// If the VersionOrUrl is a Version specifier and any of the specifiers contains a
    /// prerelease, then pre-releases are allowed. For example,
    /// `jupyterlab==3.0.0a1` allows pre-releases, but `jupyterlab==3.0.0` does not.
    ///
    /// We pre-compute if any of the items in the specifiers contains a pre-release and store
    /// this as a boolean which is later used during matching.
    allows_prerelease: bool,
}

impl PypiVersionSet {
    /// Create a PyPiVersionSeet from VersionOrUrl specifier
    pub fn from_spec(spec: Option<VersionOrUrl>, prerelease_option: &PreReleaseResolution) -> Self {
        let allows_prerelease = match prerelease_option {
            PreReleaseResolution::Disallow => false,
            PreReleaseResolution::AllowIfNoOtherVersionsOrEnabled { .. } => match spec.as_ref() {
                Some(VersionOrUrl::VersionSpecifier(v)) => {
                    v.iter().any(|s| s.version().any_prerelease())
                }
                _ => false,
            },
            PreReleaseResolution::Allow => true,
        };

        Self {
            spec,
            allows_prerelease,
        }
    }
}

impl Display for PypiVersionSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.spec {
            None => write!(f, "*"),
            Some(VersionOrUrl::Url(url)) => write!(f, "{url}"),
            Some(VersionOrUrl::VersionSpecifier(spec)) => write!(f, "{spec}"),
        }
    }
}

/// This is a wrapper around [`Version`] that serves a version
/// within the [`PypiVersionSet`] version set.
#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum PypiVersion {
    /// Version of artifact
    Version {
        /// Version of Artifact
        version: Version,

        /// Given that the [`PreReleaseResolution`] is
        /// AllowIfNoOtherVersionsOrEnabled, this field is true if there are
        /// only pre-releases available for this package or if a spec explicitly
        /// enabled pre-releases for this package. For example, if the package
        /// `foo` has only versions `foo-1.0.0a1` and `foo-1.0.0a2` then this
        /// will be true. This allows us later to match against this version and
        /// allow the selection of pre-releases. Additionally, this is also true
        /// if any of the explicitly mentioned specs (by the user) contains a
        /// prerelease (for example c>0.0.0b0) contains the `b0` which signifies
        /// a pre-release.
        package_allows_prerelease: bool,
    },
    /// Direct reference for artifact
    Url(Url),
    /// Virtual solvable representing an extra feature
    /// This is used to model conditional dependencies (e.g., requests[socks])
    Extra {
        /// The base package name
        package: NormalizedPackageName,
        /// The extra name
        extra: Extra,
    },
}

impl PypiVersion {
    /// Return if there are any prereleases for version
    pub fn any_prerelease(&self) -> bool {
        match self {
            PypiVersion::Url(_) => false,
            PypiVersion::Version { version, .. } => version.any_prerelease(),
            PypiVersion::Extra { .. } => false,
        }
    }

    /// Return if pypi version is git url version
    pub fn is_git(&self) -> bool {
        match self {
            PypiVersion::Version { .. } => false,
            PypiVersion::Url(url) => url.scheme().contains("git"),
            PypiVersion::Extra { .. } => false,
        }
    }
}

impl VersionSet for PypiVersionSet {
    type V = PypiVersion;
}

impl PypiVersionSet {
    /// Returns true if the given version is contained in this version set.
    pub fn contains(&self, v: &PypiVersion) -> bool {
        match (self.spec.as_ref(), v) {
            (Some(VersionOrUrl::Url(a)), PypiVersion::Url(b)) => a.given() == Some(b.as_str()),
            (
                Some(VersionOrUrl::VersionSpecifier(spec)),
                PypiVersion::Version {
                    version,
                    package_allows_prerelease,
                },
            ) => {
                spec.contains(version)
                    // pre-releases are allowed only when the versionset allows them (jupyterlab==3.0.0a1)
                    // or there are no other versions available (foo-1.0.0a1, foo-1.0.0a2)
                    // or alternatively if the user has enabled all pre-releases or this specific (this is encoded in the allows_prerelease field)
                    && (self.allows_prerelease || *package_allows_prerelease || !version.any_prerelease())
            }
            (
                None,
                PypiVersion::Version {
                    version,
                    package_allows_prerelease,
                },
            ) => self.allows_prerelease || *package_allows_prerelease || !version.any_prerelease(),
            (None, PypiVersion::Url(_)) => true,
            // Extra features always match when spec is None (used for extra version sets)
            (None, PypiVersion::Extra { .. }) => true,
            _ => false,
        }
    }
}

impl Display for PypiVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PypiVersion::Version { version, .. } => write!(f, "{version}"),
            PypiVersion::Url(u) => write!(f, "{u}"),
            PypiVersion::Extra { package, extra } => write!(f, "{}[{}]", package, extra.as_str()),
        }
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
/// Represents a PyPI package or extra "feature"
/// Extras are modeled as virtual solvables that enable conditional dependencies
pub(crate) enum PypiPackageName {
    /// A real package
    Package(NormalizedPackageName),
    /// A virtual "extra feature" solvable for conditional dependencies
    /// Format: package_name + "__extra__" + extra_name
    ExtraFeature(NormalizedPackageName, Extra),
}

impl PypiPackageName {
    /// Create a new package name
    pub fn package(name: NormalizedPackageName) -> Self {
        Self::Package(name)
    }

    /// Create a new extra feature name
    pub fn extra_feature(package: NormalizedPackageName, extra: Extra) -> Self {
        Self::ExtraFeature(package, extra)
    }

    /// Returns the base package name
    pub fn base_package(&self) -> &NormalizedPackageName {
        match self {
            PypiPackageName::Package(name) => name,
            PypiPackageName::ExtraFeature(name, _) => name,
        }
    }

    /// Returns true if this is an extra feature
    pub fn is_extra_feature(&self) -> bool {
        matches!(self, PypiPackageName::ExtraFeature(_, _))
    }
}

impl From<NormalizedPackageName> for PypiPackageName {
    fn from(name: NormalizedPackageName) -> Self {
        Self::Package(name)
    }
}

impl Display for PypiPackageName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PypiPackageName::Package(name) => write!(f, "{}", name),
            PypiPackageName::ExtraFeature(name, extra) => {
                write!(f, "{}[{}]", name, extra.as_str())
            }
        }
    }
}
