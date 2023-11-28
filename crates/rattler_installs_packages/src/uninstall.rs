//! Functionality to remove python distributions from an environment.

use crate::types::Record;
use std::path::Path;
use thiserror::Error;

/// An error that can occur during the uninstallation of a python distribution.
///
/// See [`uninstall_distribution`].
#[derive(Debug, Error)]
pub enum UninstallDistributionError {
    /// The `RECORD` file is missing in the .dist-info folder. This might be because the
    /// installation previously failed or because the installer did not write a RECORD file. Either
    /// way, we cannot uninstall the distribution because there is no way to tell which files
    /// belong to this package.
    #[error("the RECORD file is missing")]
    RecordFileMissing,

    /// The `RECORD` file is invalid.
    #[error("the RECORD file is invalid")]
    RecordFileInvalid(#[from] csv::Error),

    /// Failed to delete a file
    #[error("failed to delete {0}")]
    FailedToDeleteFile(String, #[source] std::io::Error),
}

/// Uninstall a python distribution from an environment
///
/// * site_packages_dir: The absolute path to the site-packages directory
/// * dist_info_dir: The path off the `.dist-info` dir relative to `site_packages_dir`.
///
/// This function will delete all the files specified in the `RECORD` file of the distribution.
pub fn uninstall_distribution(
    site_packages_dir: &Path,
    dist_info_dir: &Path,
) -> Result<(), UninstallDistributionError> {
    // Load the RECORD file
    let record = match Record::from_path(&site_packages_dir.join(dist_info_dir).join("RECORD")) {
        Ok(record) => record,
        Err(e) => {
            return Err(match e.kind() {
                csv::ErrorKind::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Special case, if the file could not be found we return a different error
                    UninstallDistributionError::RecordFileMissing
                }
                _ => UninstallDistributionError::RecordFileInvalid(e),
            });
        }
    };

    // Delete all the files specified in the RECORD file
    for entry in record.into_iter() {
        let entry_path = site_packages_dir.join(&entry.path);
        if let Err(e) = std::fs::remove_file(entry_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(UninstallDistributionError::FailedToDeleteFile(
                    entry.path, e,
                ));
            }
        }
    }

    // TODO: Should we also remove empty directories?

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::types::RecordEntry;
    use tempfile::tempdir;

    #[test]
    fn test_uninstall_distribution() {
        let temp_dir = tempdir().unwrap();
        let site_packages_dir = temp_dir.path().join("site-packages");
        std::fs::create_dir(&site_packages_dir).unwrap();
        let dist_info_dir = Path::new("test-1.0.0.dist-info");
        std::fs::create_dir(&site_packages_dir.join(dist_info_dir)).unwrap();

        let files = [
            "test-1.0.0.dist-info/RECORD",
            "test-1.0.0.dist-info/METADATA",
            "test/__init__.py",
            "test/__main__.py",
            "test/__pycache__/__main__.cpython-39.pyc",
            "test/__pycache__/__init__.cpython-39.pyc",
        ];

        // Create a RECORD file
        let record = Record::from_iter(files.map(|path| RecordEntry {
            path: path.to_string(),
            hash: None,
            size: None,
        }));

        // Create the files
        for entry in record.iter() {
            let path = site_packages_dir.join(&entry.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::File::create(path).unwrap();
        }

        // Overwrite the RECORD file
        record
            .write_to_path(&site_packages_dir.join(dist_info_dir).join("RECORD"))
            .unwrap();

        // Uninstall the distribution
        uninstall_distribution(&site_packages_dir, dist_info_dir).unwrap();

        // Check that all files are gone
        for entry in record.iter() {
            let path = site_packages_dir.join(&entry.path);
            assert!(!path.exists(), "{} still remains!", entry.path);
        }
    }
}
