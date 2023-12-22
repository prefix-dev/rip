use itertools::Itertools;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

/// Error that can occur while finding the python executable.
#[derive(Debug, Error)]
pub enum FindPythonError {
    #[error("could not find python executable")]
    NotFound,
    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

/// Try to find the python executable in the current environment.
/// Using sys.executable aproach will return original interpretator path
/// and not the shim in case of using which
pub fn system_python_executable() -> Result<PathBuf, FindPythonError> {
    // When installed with homebrew on macOS, the python3 executable is called `python3` instead
    // Also on some ubuntu installs this is the case
    // For windows it should just be python

    let output = match std::process::Command::new("python3")
        .arg("-c")
        .arg("import sys; print(sys.executable, end='')")
        .output()
        .or_else(|_| {
            std::process::Command::new("python")
                .arg("-c")
                .arg("import sys; print(sys.executable, end='')")
                .output()
        }) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Err(FindPythonError::NotFound),
        Err(e) => return Err(FindPythonError::IoError(e)),
        Ok(output) => output,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let python_path = PathBuf::from_str(&stdout).unwrap();

    // sys.executable can return empty string or python's None
    if !python_path.exists() {
        return Err(FindPythonError::NotFound);
    }

    Ok(python_path)
}

/// Errors that can occur while trying to parse the python version
#[derive(Debug, Error)]
pub enum ParsePythonInterpreterVersionError {
    #[error("failed to parse version string, found '{0}' expect something like 'Python x.x.x'")]
    InvalidVersion(String),
    #[error(transparent)]
    FindPythonError(#[from] FindPythonError),
}

#[derive(Clone)]
pub struct PythonInterpreterVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl From<(u32, u32, u32)> for PythonInterpreterVersion {
    fn from(value: (u32, u32, u32)) -> Self {
        Self {
            major: value.0,
            minor: value.1,
            patch: value.2,
        }
    }
}

impl PythonInterpreterVersion {
    /// Get the version of the python interpreter
    /// Expects the string from `python --version` as input
    /// getting something along the lines of `Python 3.8.5`
    pub fn from_python_output(
        version_str: &str,
    ) -> Result<Self, ParsePythonInterpreterVersionError> {
        use ParsePythonInterpreterVersionError::InvalidVersion;

        // Split "Python 3.9.1" into "Python" and "3.9.1"
        let version_str = match version_str.split_once(' ') {
            Some(("Python", version)) => version,
            _ => return Err(InvalidVersion(version_str.to_owned())),
        };

        // Split the version into strings separated by '.' and parse them
        let parts = version_str
            .split('.')
            .map(str::trim)
            .map(FromStr::from_str)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| InvalidVersion(version_str.to_owned()))?;

        // Extract the major, minor and patch version
        let Some((major, minor, patch)) = parts.into_iter().collect_tuple() else {
            return Err(InvalidVersion(version_str.to_owned()));
        };

        Ok(Self::new(major, minor, patch))
    }

    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Get the python version from the system interpreter
    pub fn from_system() -> Result<Self, ParsePythonInterpreterVersionError> {
        Self::from_path(&system_python_executable()?)
    }

    /// Get the python version a path to the python executable
    pub fn from_path(path: &Path) -> Result<Self, ParsePythonInterpreterVersionError> {
        let output = std::process::Command::new(path)
            .arg("--version")
            .output()
            .map_err(|_| FindPythonError::NotFound)?;
        let version_str = String::from_utf8_lossy(&output.stdout);
        Self::from_python_output(&version_str)
    }
}

#[cfg(test)]
mod tests {
    use crate::python_env::PythonInterpreterVersion;

    #[test]
    pub fn parse_python_version() {
        let version = PythonInterpreterVersion::from_python_output("Python 3.8.5\n").unwrap();
        assert_eq!(version.major, 3);
        assert_eq!(version.minor, 8);
        assert_eq!(version.patch, 5);
    }
}
