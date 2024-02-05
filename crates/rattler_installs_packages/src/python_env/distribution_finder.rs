//! This module implements logic to locate so called python distributions (installed artifacts)
//! in an environment.
//!
//! The implementation is based on the <https://packaging.python.org/en/latest/specifications/recording-installed-packages>
//! which is based on [PEP 376](https://peps.python.org/pep-0376/) and [PEP 627](https://peps.python.org/pep-0627/).

use crate::artifacts::wheel::InstallPaths;
use crate::python_env::WheelTag;
use crate::{types::NormalizedPackageName, types::PackageName, types::RFC822ish};
use fs_err as fs;
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

/// Locates the python distributions (packages) that have been installed in the specified directory.
///
/// When packages are installed in a venv they are installed in specific directories. Use the
/// [`find_distributions_in_venv`] if you don't want to deal with determining the proper directory.
///
/// Any path in the results is relative to `search_dir`.
pub fn find_distributions_in_directory(
    search_dir: &Path,
) -> Result<Vec<Distribution>, FindDistributionError> {
    let mut result = Vec::new();
    for entry in search_dir.read_dir()? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(dist) = analyze_distribution(entry.path())? {
                result.push(Distribution {
                    dist_info: pathdiff::diff_paths(&dist.dist_info, search_dir)
                        .unwrap_or(dist.dist_info),
                    ..dist
                })
            }
        }
    }

    Ok(result)
}

/// Locates the python distributions (packages) that have been installed in the virtualenv rooted at
/// `root`.
pub fn find_distributions_in_venv(
    root: &Path,
    paths: &InstallPaths,
) -> Result<Vec<Distribution>, FindDistributionError> {
    // We will look for distributions in the purelib/platlib directories
    let locations = [paths.purelib(), paths.platlib()]
        .into_iter()
        .map(|p| root.join(p))
        .unique()
        .filter(|p| p.is_dir());

    let mut results = Vec::new();
    for dir in locations {
        // Find distributions in the directory.
        let distributions = find_distributions_in_directory(&dir)?;

        // Modify the paths in the result to be relative to the root of the environment instead of
        // to the search directory.
        let dir_relative_path = pathdiff::diff_paths(&dir, root).unwrap_or_else(|| dir.clone());
        results.extend(distributions.into_iter().map(|dist| Distribution {
            // Make the paths relative to the root
            dist_info: dir_relative_path.join(dist.dist_info),
            ..dist
        }));
    }

    Ok(results)
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
    let installer = fs::read_to_string(dist_info_path.join("INSTALLER"))
        .map(|i| i.trim().to_owned())
        .ok();

    // Check if there is a WHEEL file from where we can read tags
    let wheel_path = dist_info_path.join("WHEEL");
    let tags = if wheel_path.is_file() {
        let mut parsed = RFC822ish::from_str(&fs::read_to_string(&wheel_path)?)
            .map_err(move |e| FindDistributionError::FailedToParseWheel(wheel_path, e))?;

        Some(
            parsed
                .take_all("Tag")
                .into_iter()
                .map(|tag| {
                    WheelTag::from_compound_string(&tag)
                        .map_err(|_| FindDistributionError::FailedToParseWheelTag(tag))
                })
                .flatten_ok()
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

    #[test]
    fn test_find_distributions() {
        // Describe the virtual environment
        let venv_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/find_distributions/");
        let install_paths = InstallPaths::for_venv((3, 8, 5), true);

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
