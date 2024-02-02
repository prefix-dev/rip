use super::{NormalizedPackageName, PackageName, ParsePackageNameError};
use crate::python_env::WheelTag;
use crate::types::Version;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use thiserror::Error;
use url::Url;

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
#[derive(Debug, Clone, PartialOrd, Ord, Eq, PartialEq, Serialize, Deserialize)]
pub enum ArtifactName {
    /// Wheel artifact
    Wheel(WheelFilename),
    /// Sdist artifact
    SDist(SDistFilename),
    /// STree artifact
    STree(STreeFilename),
}

impl ArtifactName {
    /// Returns the version of the artifact
    pub fn version(&self) -> Version {
        match self {
            ArtifactName::Wheel(name) => name.version.clone(),
            ArtifactName::SDist(name) => name.version.clone(),
            ArtifactName::STree(name) => name.version.clone(),
        }
    }

    /// Returns this name as a wheel name
    pub fn as_wheel(&self) -> Option<&WheelFilename> {
        match self {
            ArtifactName::Wheel(wheel) => Some(wheel),
            ArtifactName::SDist(_) => None,
            ArtifactName::STree(_) => None,
        }
    }

    /// Returns this name as a wheel name
    pub fn as_sdist(&self) -> Option<&SDistFilename> {
        match self {
            ArtifactName::Wheel(_) => None,
            ArtifactName::STree(_) => None,
            ArtifactName::SDist(sdist) => Some(sdist),
        }
    }

    /// Returns this name as a source tree name
    pub fn as_stree(&self) -> Option<&STreeFilename> {
        match self {
            ArtifactName::Wheel(_) => None,
            ArtifactName::STree(name) => Some(name),
            ArtifactName::SDist(_) => None,
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
            ArtifactName::STree(name) => write!(f, "{}", name),
        }
    }
}

/// Structure that contains the information that is contained in a wheel name
/// See: [File Name Convention](https://www.python.org/dev/peps/pep-0427/#file-name-convention),
/// and: [PyPA Conventions](https://packaging.python.org/en/latest/specifications/),
/// for more details regarding the structure of a wheel name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct WheelFilename {
    /// Distribution name, e.g. ‘django’, ‘pyramid’.
    pub distribution: PackageName,

    /// Distribution version, e.g. 1.0.
    pub version: Version,

    /// Optional build number.
    pub build_tag: Option<BuildTag>,

    /// Language implementation and version tag
    /// E.g. ‘py27’, ‘py2’, ‘py3’.
    pub py_tags: Vec<String>,

    /// ABI specific tags
    /// E.g. ‘cp33m’, ‘abi3’, ‘none’.
    pub abi_tags: Vec<String>,

    /// Architecture specific tags
    /// E.g. ‘linux_x86_64’, ‘any’, ‘manylinux_2_17_x86_64’
    pub arch_tags: Vec<String>,
}

impl WheelFilename {
    /// Creates a set of all tags that are contained in this wheel name.
    pub fn all_tags(&self) -> HashSet<WheelTag> {
        HashSet::from_iter(self.all_tags_iter())
    }

    /// Returns an iterator over all the tags that are contained in this wheel name. Note that there
    /// might be duplicates in the iterator. Use [`Self::all_tags`] if you want a unique set of
    /// tags.
    pub fn all_tags_iter(&self) -> impl Iterator<Item = WheelTag> + '_ {
        self.py_tags
            .iter()
            .cartesian_product(self.abi_tags.iter())
            .cartesian_product(self.arch_tags.iter())
            .map(|((py, abi), arch)| WheelTag {
                interpreter: py.clone(),
                abi: abi.clone(),
                platform: arch.clone(),
            })
    }
}

impl Display for WheelFilename {
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
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, SerializeDisplay, DeserializeFromStr)]
pub struct BuildTag {
    number: u32,
    name: String,
}

impl Display for BuildTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.number, &self.name)
    }
}

/// Structure that contains the information that is contained in a source distribution name
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Serialize, Deserialize)]
pub struct SDistFilename {
    /// Distribution name, e.g. ‘django’, ‘pyramid’.
    pub distribution: PackageName,

    /// Distribution version, e.g. 1.0.
    pub version: Version,

    /// The format of the file
    pub format: SDistFormat,
}

/// Structure that contains the information that is contained in a source distribution name
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Serialize, Deserialize)]
pub struct STreeFilename {
    /// Distribution name, e.g. ‘django’, ‘pyramid’.
    pub distribution: PackageName,

    /// Resolved version of source tree
    pub version: Version,

    /// Direct reference
    pub url: Url,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Serialize, Deserialize)]
/// SourceArtifactName
pub enum SourceArtifactName {
    /// SDIST
    SDist(SDistFilename),
    /// STREE
    STree(STreeFilename),
}

impl Display for SourceArtifactName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceArtifactName::SDist(sdist) => write!(f, "{}", sdist),
            SourceArtifactName::STree(stree) => write!(f, "{}", stree),
        }
    }
}

impl Display for SDistFilename {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{dist}-{ver}{format}",
            dist = self.distribution.as_source_str(),
            ver = self.version,
            format = self.format,
        )
    }
}

impl Display for STreeFilename {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{dist}-{ver}",
            dist = self.distribution.as_source_str(),
            ver = self.version,
        )
    }
}

/// Describes the format in which the source distribution is shipped.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum SDistFormat {
    Zip,
    TarGz,
    TarBz2,
    TarXz,
    TarZ,
    Tar,
}

impl SDistFormat {
    /// In RIP we currently only support TarGz and Tar
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::TarGz | Self::Tar | Self::Zip)
    }

    /// Get extension of SDist
    pub fn get_extension(path: &str) -> Result<SDistFormat, ParseArtifactNameError> {
        let format = if path.strip_suffix(".zip").is_some() {
            SDistFormat::Zip
        } else if path.strip_suffix(".tar.gz").is_some() {
            SDistFormat::TarGz
        } else if path.strip_suffix(".tar.bz2").is_some() {
            SDistFormat::TarBz2
        } else if path.strip_suffix(".tar.xz").is_some() {
            SDistFormat::TarXz
        } else if path.strip_suffix(".tar.Z").is_some() {
            SDistFormat::TarZ
        } else if path.strip_suffix(".tar").is_some() {
            SDistFormat::Tar
        } else {
            return Err(ParseArtifactNameError::InvalidExtension(path.to_string()));
        };

        Ok(format)
    }
}

impl Display for SDistFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{format}",
            format = match self {
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

/// An error that can occur when parsing an artifact name
#[derive(Debug, Clone, Error)]
#[allow(missing_docs)]
pub enum ParseArtifactNameError {
    #[error("invalid artifact name")]
    InvalidName,

    #[error("package name '{0}' not found in filename: '{1}'")]
    PackageNameNotFound(NormalizedPackageName, String),

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

impl SDistFilename {
    /// Parse the sdist name from a filename string
    /// e.g "trio-0.18.0.tar.gz"
    pub fn from_filename(
        s: &str,
        normalized_package_name: &NormalizedPackageName,
    ) -> Result<Self, ParseArtifactNameError> {
        let (package_name, rest) = split_into_filename_rest(s, normalized_package_name).ok_or(
            ParseArtifactNameError::PackageNameNotFound(
                normalized_package_name.clone(),
                s.to_string(),
            ),
        )?;

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
        let version = Version::from_str(version)
            .map_err(|e| ParseArtifactNameError::InvalidVersion(e.to_string()))?;

        Ok(SDistFilename {
            distribution,
            version,
            format,
        })
    }
}

/// Split the filename into a filename and the rest of the path
/// by matching it with the normalized package name.
/// Split on the `-` and check if the first part is the normalized package name.
/// Otherwise continue splitting on the `-` until we find the normalized package name.
///
/// E.g `trio-0.18.0-py3-none-any.whl` with normalized package name `trio`
/// should split into (`trio`, `0.18.0-py3-none-any.whl`)
fn split_into_filename_rest<'a>(
    s: &'a str,
    normalized_package_name: &NormalizedPackageName,
) -> Option<(&'a str, &'a str)> {
    for (idx, char) in s.char_indices() {
        if char == '-' {
            let (name, rest) = (&s[..idx], &s[idx + 1..]);
            let parsed = name.parse::<NormalizedPackageName>();
            if let Ok(parsed) = parsed {
                if parsed == *normalized_package_name {
                    return Some((name, rest));
                }
            }
        }
    }
    None
}

impl WheelFilename {
    /// Parse the wheel name from a filename string
    /// e.g "trio-0.18.0-py3-none-any.whl"
    pub fn from_filename(
        s: &str,
        normalized_package_name: &NormalizedPackageName,
    ) -> Result<Self, ParseArtifactNameError> {
        let Some(file_stem) = s.strip_suffix(".whl") else {
            return Err(ParseArtifactNameError::InvalidExtension(s.to_string()));
        };

        // Parse the distribution
        let Some((distribution, rest)) =
            split_into_filename_rest(file_stem, normalized_package_name)
        else {
            return Err(ParseArtifactNameError::PackageNameNotFound(
                normalized_package_name.clone(),
                s.to_string(),
            ));
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

impl ArtifactName {
    /// Parse the artifact name for a filename string
    /// e.g "trio-0.18.0-py3-none-any.whl"
    /// it uses the normalized package name to check where to split the string
    pub fn from_filename(
        input: &str,
        normalized_package_name: &NormalizedPackageName,
    ) -> Result<Self, ParseArtifactNameError> {
        if input.ends_with(".whl") {
            Ok(ArtifactName::Wheel(WheelFilename::from_filename(
                input,
                normalized_package_name,
            )?))
        } else if input.ends_with(".zip")
            || input.ends_with(".tar.gz")
            || input.ends_with(".tar.bz2")
            || input.ends_with(".tar.xz")
            || input.ends_with(".tar.Z")
            || input.ends_with(".tar")
        {
            Ok(ArtifactName::SDist(SDistFilename::from_filename(
                input,
                normalized_package_name,
            )?))
        } else {
            Err(ParseArtifactNameError::InvalidExtension(input.to_string()))
        }
    }
}

/// A trait to convert the general [`ArtifactName`] into a specialized artifact name. This is useful
/// to generically fetch the underlying specialized name.
///
/// Currently we provide implementations for wheels and sdists.
pub trait InnerAsArtifactName {
    /// Tries to convert the general [`ArtifactName`] into a specialized artifact name.
    fn try_as(name: &ArtifactName) -> Option<&Self>;
}

impl InnerAsArtifactName for WheelFilename {
    fn try_as(name: &ArtifactName) -> Option<&Self> {
        name.as_wheel()
    }
}

impl InnerAsArtifactName for SDistFilename {
    fn try_as(name: &ArtifactName) -> Option<&Self> {
        name.as_sdist()
    }
}

impl InnerAsArtifactName for STreeFilename {
    fn try_as(name: &ArtifactName) -> Option<&Self> {
        dbg!("get name");
        name.as_stree()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_filename_splitting() {
        let normalized_package_name = NormalizedPackageName::from_str("trio").unwrap();
        let filename = "trio-0.18.0-py3-none-any.whl";
        let (name, rest) = split_into_filename_rest(filename, &normalized_package_name).unwrap();
        assert_eq!(name, "trio");
        assert_eq!(rest, "0.18.0-py3-none-any.whl");

        let normalized_package_name = NormalizedPackageName::from_str("trio-three").unwrap();
        let filename = "trio-three-0.18.0-py3-none-any.whl";
        let (name, rest) = split_into_filename_rest(filename, &normalized_package_name).unwrap();
        assert_eq!(name, "trio-three");
        assert_eq!(rest, "0.18.0-py3-none-any.whl");
    }

    #[test]
    fn test_sdist_name_from_str() {
        let sn =
            SDistFilename::from_filename("trio-0.19a0.tar.gz", &"trio".parse().unwrap()).unwrap();
        assert_eq!(sn.distribution, "trio".parse().unwrap());
        assert_eq!(sn.version, "0.19a0".parse().unwrap());

        assert_eq!(sn.to_string(), "trio-0.19a0.tar.gz");

        let sn = SDistFilename::from_filename(
            "create_ap-gui-1.3.1.tar.gz",
            &"create_ap-gui".parse().unwrap(),
        )
        .unwrap();

        assert_eq!(sn.distribution, "create_ap-gui".parse().unwrap());
        assert_eq!(sn.version, "1.3.1".parse().unwrap());
    }

    #[test]
    fn test_name_double_dash_from_str() {
        let sn = SDistFilename::from_filename(
            "trio-three-0.19a0.tar.gz",
            &"trio-three".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(sn.distribution, "trio-three".parse().unwrap());
        assert_eq!(sn.version, "0.19a0".parse().unwrap());

        assert_eq!(sn.to_string(), "trio-three-0.19a0.tar.gz");
    }

    #[test]
    fn test_many_linux() {
        let n = WheelFilename::from_filename(
            "numpy-1.26.0-pp39-pypy39_pp73-manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
            &"numpy".parse().unwrap(),
        )
        .unwrap();

        assert_eq!(
            n.arch_tags,
            vec!["manylinux_2_17_x86_64", "manylinux2014_x86_64"]
        );
    }

    #[test]
    fn test_wheel_name_from_str() {
        let n =
            WheelFilename::from_filename("trio-0.18.0-py3-none-any.whl", &"trio".parse().unwrap())
                .unwrap();
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
        let n = WheelFilename::from_filename(
            "foo.bar-0.1b3-1local-py2.py3-none-any.whl",
            &"foo.bar".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(n.distribution, "foo.bar".parse().unwrap());
        assert_eq!(n.version, "0.1b3".parse().unwrap());
        assert_eq!(
            n.build_tag,
            Some(BuildTag {
                number: 1,
                name: String::from("local"),
            })
        );
        assert_eq!(n.py_tags, vec!["py2", "py3"],);
        assert_eq!(n.abi_tags, vec!["none"]);
        assert_eq!(n.arch_tags, vec!["any"]);

        assert_eq!(n.to_string(), "foo.bar-0.1b3-1local-py2.py3-none-any.whl");
    }
}
