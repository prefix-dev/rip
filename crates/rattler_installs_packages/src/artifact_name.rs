use crate::package_name::{PackageName, ParsePackageNameError};
use pep440::Version;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use thiserror::Error;

/// The [`ArtifactName`] enum represents a package artifact name and the properties that can be
/// derived simply from the name.
///
/// An artifact is a packaged form of a software project that can be easily distributed and
/// installed. In the context of this enum, an artifact can be either a wheel or a source
/// distribution (sdist).
///
/// A wheel is a binary distribution format specific to Python. It contains pre-compiled code
/// that can be directly installed on compatible systems, eliminating the need for compilation.
/// Wheels provide faster installation and better compatibility, especially for binary dependencies.
///
/// On the other hand, a source distribution (sdist) is a package format that includes the source
/// code of the software project, along with any required build scripts or configuration files.
/// Source distributions are platform-independent and can be built and installed on various systems,
/// but they require compilation and might have additional dependencies that need to be resolved.
///
/// The `ArtifactName` enum allows distinguishing between these two types of artifacts,
/// providing flexibility and clarity when working with Python package distributions.
#[derive(Debug, Clone, PartialOrd, Ord, Eq, PartialEq, DeserializeFromStr, SerializeDisplay)]
pub enum ArtifactName {
    Wheel(WheelName),
    SDist(SDistName),
}

impl ArtifactName {
    /// Returns the version of the artifact
    pub fn version(&self) -> &Version {
        match self {
            ArtifactName::Wheel(name) => &name.version,
            ArtifactName::SDist(name) => &name.version,
        }
    }

    /// Returns this name as a wheel name
    pub fn as_wheel(&self) -> Option<&WheelName> {
        match self {
            ArtifactName::Wheel(wheel) => Some(wheel),
            ArtifactName::SDist(_) => None,
        }
    }

    /// Returns this name as a wheel name
    pub fn as_sdist(&self) -> Option<&SDistName> {
        match self {
            ArtifactName::Wheel(_) => None,
            ArtifactName::SDist(sdist) => Some(sdist),
        }
    }

    /// Tries to convert the specialized instance
    pub fn as_inner<T: InnerAsArtifactName>(&self) -> Option<&T> {
        T::try_as(self)
    }
}

impl Display for ArtifactName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ArtifactName::Wheel(name) => write!(f, "{}", name),
            ArtifactName::SDist(name) => write!(f, "{}", name),
        }
    }
}

// https://packaging.python.org/specifications/binary-distribution-format/#file-name-convention
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct WheelName {
    /// Distribution name, e.g. ‘django’, ‘pyramid’.
    pub distribution: PackageName,

    /// Distribution version, e.g. 1.0.
    pub version: Version,

    /// Optional build number.
    pub build_tag: Option<BuildTag>,

    /// Language implementation and version tag
    pub py_tags: Vec<String>,

    pub abi_tags: Vec<String>,

    pub arch_tags: Vec<String>,
}

impl WheelName {
    pub fn all_tags(&self) -> HashSet<String> {
        let mut retval = HashSet::new();
        for py in &self.py_tags {
            for abi in &self.abi_tags {
                for arch in &self.arch_tags {
                    retval.insert(format!("{}-{}-{}", py, abi, arch));
                }
            }
        }
        retval
    }
}

impl Display for WheelName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{dist}-{ver}{build}-{py_tags}-{abi_tags}-{arch_tags}.whl",
            dist = self.distribution.as_source_str(),
            ver = self.version,
            build = self
                .build_tag
                .as_ref()
                .map_or_else(|| String::from(""), |tag| format!("-{tag}")),
            py_tags = self.py_tags.join("."),
            abi_tags = self.abi_tags.join("."),
            arch_tags = self.arch_tags.join("."),
        )
    }
}

/// A build number. Must start with a digit. Acts as a tie-breaker if two wheel file names are the
/// same in all other respects (i.e. name, version and other tags).
///
/// Sort as an empty tuple if unspecified, else sort as a two-item tuple with the first item being
/// the initial digits as an int, and the second item being the remainder of the tag as a str.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct BuildTag {
    number: u32,
    name: String,
}

impl Display for BuildTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.number, &self.name)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SDistName {
    /// Distribution name, e.g. ‘django’, ‘pyramid’.
    pub distribution: PackageName,

    /// Distribution version, e.g. 1.0.
    pub version: Version,

    /// The format of the file
    pub format: SDistFormat,
}

impl Display for SDistName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{dist}-{ver}{format}",
            dist = self.distribution.as_source_str(),
            ver = self.version,
            format = match self.format {
                SDistFormat::Zip => ".zip",
                SDistFormat::TarGz => ".tar.gz",
                SDistFormat::TarBz2 => ".tar.bz2",
                SDistFormat::TarXz => ".tar.xz",
                SDistFormat::TarZ => ".tar.Z",
                SDistFormat::Tar => ".tar",
            }
        )
    }
}

/// Describes the format in which the source distribution is shipped.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SDistFormat {
    Zip,
    TarGz,
    TarBz2,
    TarXz,
    TarZ,
    Tar,
}

#[derive(Debug, Clone, Error)]
pub enum ParseArtifactNameError {
    #[error("invalid artifact name")]
    InvalidName,

    #[error("invalid artifact extension. Must be either .whl, .tar.gz, or .zip (filename='{0}')")]
    InvalidExtension(String),

    #[error(transparent)]
    InvalidPackageName(#[from] ParsePackageNameError),

    #[error("invalid version: '{0}'")]
    InvalidVersion(String),

    #[error("build tag '{0}' must start with a digit")]
    BuildTagMustStartWithDigit(String),
}

impl FromStr for BuildTag {
    type Err = ParseArtifactNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let first_alpha_idx = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
        let (digits, name) = s.split_at(first_alpha_idx);
        Ok(Self {
            number: digits
                .parse()
                .map_err(|_| ParseArtifactNameError::BuildTagMustStartWithDigit(s.to_owned()))?,
            name: name.to_owned(),
        })
    }
}

impl FromStr for SDistName {
    type Err = ParseArtifactNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (package_name, rest) = s
            .split_once('-')
            .ok_or(ParseArtifactNameError::InvalidName)?;

        // Determine the package format
        let (version, format) = if let Some(rest) = rest.strip_suffix(".zip") {
            (rest, SDistFormat::Zip)
        } else if let Some(rest) = rest.strip_suffix(".tar.gz") {
            (rest, SDistFormat::TarGz)
        } else if let Some(rest) = rest.strip_suffix(".tar.bz2") {
            (rest, SDistFormat::TarBz2)
        } else if let Some(rest) = rest.strip_suffix(".tar.xz") {
            (rest, SDistFormat::TarXz)
        } else if let Some(rest) = rest.strip_suffix(".tar.Z") {
            (rest, SDistFormat::TarZ)
        } else if let Some(rest) = rest.strip_suffix(".tar") {
            (rest, SDistFormat::Tar)
        } else {
            return Err(ParseArtifactNameError::InvalidExtension(rest.to_string()));
        };

        // Parse the package name
        let distribution = PackageName::from_str(package_name)
            .map_err(ParseArtifactNameError::InvalidPackageName)?;

        // Parse the version
        let version = pep440::Version::from_str(version)
            .map_err(|e| ParseArtifactNameError::InvalidVersion(e.to_string()))?;

        Ok(SDistName {
            distribution,
            version,
            format,
        })
    }
}

impl FromStr for WheelName {
    type Err = ParseArtifactNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some(file_stem) = s.strip_suffix(".whl") else {
            return Err(ParseArtifactNameError::InvalidExtension(s.to_string()));
        };

        // Parse the distribution
        let Some((distribution, rest)) = file_stem.split_once('-') else {
            return Err(ParseArtifactNameError::InvalidName);
        };
        let distribution = PackageName::from_str(distribution)
            .map_err(ParseArtifactNameError::InvalidPackageName)?;

        // Parse the version
        let Some((version, rest)) = rest.split_once('-') else {
            return Err(ParseArtifactNameError::InvalidName);
        };
        let version = Version::from_str(version)
            .map_err(|e| ParseArtifactNameError::InvalidVersion(e.to_string()))?;

        // Parse the platform tag
        let Some((rest, platform_tags)) = rest.rsplit_once('-') else {
            return Err(ParseArtifactNameError::InvalidName);
        };
        let arch_tags = platform_tags.split('.').map(ToOwned::to_owned).collect();

        // Parse the abi tag
        let Some((rest, abi_tag)) = rest.rsplit_once('-') else {
            return Err(ParseArtifactNameError::InvalidName);
        };
        let abi_tags = abi_tag.split('.').map(ToOwned::to_owned).collect();

        // Parse the python tag
        let (build_tag, python_tag) = match rest.rsplit_once('-') {
            Some((build_tag, python_tag)) => (Some(build_tag), python_tag),
            None => (None, rest),
        };
        let py_tags = python_tag.split('.').map(ToOwned::to_owned).collect();
        let build_tag = build_tag
            .map(BuildTag::from_str)
            .map_or_else(|| Ok(None), |result| result.map(Some))?;

        Ok(Self {
            distribution,
            version,
            build_tag,
            py_tags,
            abi_tags,
            arch_tags,
        })
    }
}

impl FromStr for ArtifactName {
    type Err = ParseArtifactNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.ends_with(".whl") {
            Ok(ArtifactName::Wheel(WheelName::from_str(s)?))
        } else if s.ends_with(".zip")
            || s.ends_with(".tar.gz")
            || s.ends_with(".tar.bz2")
            || s.ends_with(".tar.xz")
            || s.ends_with(".tar.Z")
            || s.ends_with(".tar")
        {
            Ok(ArtifactName::SDist(SDistName::from_str(s)?))
        } else {
            Err(ParseArtifactNameError::InvalidExtension(s.to_string()))
        }
    }
}

/// A trait to convert the general [`ArtifactName`] into a specialized artifact name. This is useful
/// to generically fetch the underlying specialized name.
pub trait InnerAsArtifactName {
    fn try_as(name: &ArtifactName) -> Option<&Self>;
}

impl InnerAsArtifactName for WheelName {
    fn try_as(name: &ArtifactName) -> Option<&Self> {
        name.as_wheel()
    }
}

impl InnerAsArtifactName for SDistName {
    fn try_as(name: &ArtifactName) -> Option<&Self> {
        name.as_sdist()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_sdist_name_from_str() {
        let sn: SDistName = "trio-0.19a0.tar.gz".parse().unwrap();
        assert_eq!(sn.distribution, "trio".parse().unwrap());
        assert_eq!(sn.version, "0.19a0".parse().unwrap());

        assert_eq!(sn.to_string(), "trio-0.19a0.tar.gz");
    }

    #[test]
    fn test_wheel_name_from_str() {
        let n: WheelName = "trio-0.18.0-py3-none-any.whl".parse().unwrap();
        assert_eq!(n.distribution, "trio".parse().unwrap());
        assert_eq!(n.version, "0.18.0".parse().unwrap());
        assert_eq!(n.build_tag, None);
        assert_eq!(n.py_tags, vec!["py3"]);
        assert_eq!(n.abi_tags, vec!["none"]);
        assert_eq!(n.arch_tags, vec!["any"]);

        assert_eq!(n.to_string(), "trio-0.18.0-py3-none-any.whl");
    }

    #[test]
    fn test_wheel_name_from_str_harder() {
        let n: WheelName = "foo.bar-0.1b3-1local-py2.py3-none-any.whl".parse().unwrap();
        assert_eq!(n.distribution, "foo.bar".parse().unwrap());
        assert_eq!(n.version, "0.1b3".parse().unwrap());
        assert_eq!(
            n.build_tag,
            Some(BuildTag {
                number: 1,
                name: String::from("local")
            })
        );
        assert_eq!(n.py_tags, vec!["py2", "py3"],);
        assert_eq!(n.abi_tags, vec!["none"]);
        assert_eq!(n.arch_tags, vec!["any"]);

        assert_eq!(n.to_string(), "foo.bar-0.1b3-1local-py2.py3-none-any.whl");
    }
}
