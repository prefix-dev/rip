//! This module implements logic to locate so called python distributions (installed artifacts)
//! in an environment.
//!
//! The implementation is based on the <https://packaging.python.org/en/latest/specifications/recording-installed-packages>
//! which is based on [PEP 376](https://peps.python.org/pep-0376/) and [PEP 627](https://peps.python.org/pep-0627/).

use crate::tags::WheelTag;
use crate::{rfc822ish::RFC822ish, InstallPaths, NormalizedPackageName, PackageName};
use indexmap::IndexSet;
use itertools::Itertools;
use pep440_rs::Version;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
};
use thiserror::Error;

/// Information about a distribution found by `find_distributions_in_venv`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Distribution {
    /// The name of the distribution
    pub name: NormalizedPackageName,

    /// The version of the distribution
    pub version: Version,

    /// The installer that was responsible for installing the distribution
    pub installer: Option<String>,

    /// The path to the .dist-info directory relative to the root of the environment.
    pub dist_info: PathBuf,

    /// The specific tags of the distribution that was installed or `None` if this information
    /// could not be retrieved.
    pub tags: Option<IndexSet<WheelTag>>,
}

/// An error that can occur when running `find_distributions_in_venv`.
#[derive(Debug, Error)]
pub enum FindDistributionError {
    /// An IO error occurred
    #[error(transparent)]
    IoError(#[from] std::io::Error),

    /// Failed to parse a WHEEL file
    #[error("failed to parse '{0}'")]
    FailedToParseWheel(PathBuf, #[source] <RFC822ish as FromStr>::Err),

    /// Failed to parse WHEEL tags
    #[error("failed to parse wheel tag {0}")]
    FailedToParseWheelTag(String),
}

/// Locates the python distributions (packages) that have been installed in the virtualenv rooted at
/// `root`.
pub fn find_distributions_in_venv(
    root: &Path,
    paths: &InstallPaths,
) -> Result<Vec<Distribution>, FindDistributionError> {
    // We will look for distributions in the purelib/platlib directories
    let locations = [paths.mapping.get("purelib"), paths.mapping.get("platlib")]
        .into_iter()
        .filter_map(|p| Some(root.join(p?)))
        .unique()
        .filter(|p| p.is_dir())
        .collect_vec();

    // Iterate over all the entries in the in the locations and look for .dist-info entries.
    let mut result = Vec::new();
    for location in locations {
        for entry in location.read_dir()? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(dist) = analyze_distribution(entry.path())? {
                    result.push(Distribution {
                        dist_info: pathdiff::diff_paths(&dist.dist_info, root)
                            .unwrap_or(dist.dist_info),
                        ..dist
                    })
                }
            }
        }
    }

    Ok(result)
}

/// Analyzes a `.dist-info` directory to see if it actually contains a python distribution (package).
fn analyze_distribution(
    dist_info_path: PathBuf,
) -> Result<Option<Distribution>, FindDistributionError> {
    let Some((name, version)) = dist_info_path
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|n| n.strip_suffix(".dist-info"))
        .and_then(|n| n.split_once('-'))
    else {
        // If we are unable to parse the distribution name we just skip.
        return Ok(None);
    };

    // Check if the METADATA file is present. This is the only file that is mandatory, so if its
    // missing this folder is not a python distribution.
    if !dist_info_path.join("METADATA").is_file() {
        return Ok(None);
    }

    // Parse the name
    let Ok(name) = PackageName::from_str(name) else {
        // If the package name cannot be parsed, just skip
        return Ok(None);
    };

    // Parse the version
    let Ok(version) = Version::from_str(version) else {
        // If the version cannot be parsed, just skip
        return Ok(None);
    };

    // Try to read the INSTALLER file from the distribution directory
    let installer = std::fs::read_to_string(dist_info_path.join("INSTALLER"))
        .map(|i| i.trim().to_owned())
        .ok();

    // Check if there is a WHEEL file from where we can read tags
    let wheel_path = dist_info_path.join("WHEEL");
    let tags = if wheel_path.is_file() {
        let mut parsed = RFC822ish::from_str(&std::fs::read_to_string(&wheel_path)?)
            .map_err(move |e| FindDistributionError::FailedToParseWheel(wheel_path, e))?;
        Some(
            parsed
                .take_all("Tag")
                .into_iter()
                .map(|tag| {
                    WheelTag::from_str(&tag)
                        .map_err(|_| FindDistributionError::FailedToParseWheelTag(tag))
                })
                .collect::<Result<IndexSet<_>, _>>()?,
        )
    } else {
        None
    };

    Ok(Some(Distribution {
        dist_info: dist_info_path,
        name: name.into(),
        version,
        installer,
        tags,
    }))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::system_python::PythonInterpreterVersion;

    #[test]
    fn test_find_distributions() {
        // Describe the virtual environment
        let venv_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/find_distributions/");
        let install_paths = InstallPaths::for_venv(PythonInterpreterVersion::new(3, 8, 5), true);

        // Find all distributions
        let mut distributions = find_distributions_in_venv(&venv_path, &install_paths).unwrap();

        // Sort to get consistent ordering across platforms
        distributions.sort_by(|a, b| a.name.cmp(&b.name));

        insta::assert_ron_snapshot!(distributions, {
            "[].dist_info" => insta::dynamic_redaction(move |value, _path| {
                value.as_str().unwrap().replace('\\', "/")
            }),
        });
    }
}
