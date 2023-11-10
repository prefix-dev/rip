//! Module that helps with allowing in the creation of python virtual environments.
//! Now just use the python venv command to create the virtual environment.
//! Later on we can look into actually creating the environment by linking to the python library,
//! and creating the necessary files. See: [VEnv](https://packaging.python.org/en/latest/specifications/virtual-environments/#declaring-installation-environments-as-python-virtual-environments)
#![allow(dead_code)]
use crate::utils::{python_executable, FindPythonError};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum VEnvError {
    #[error(transparent)]
    FindPythonError(#[from] FindPythonError),
    #[error("failed to run 'python -m venv': `{0}`")]
    FailedToRun(String),
    #[error(transparent)]
    FailedToCreate(#[from] std::io::Error),
}

/// Represents a virtual environment
/// that we can install wheels into
pub struct VEnv {
    location: PathBuf,
}

impl VEnv {
    fn new(location: PathBuf) -> Self {
        Self { location }
    }
}

/// Specifies where to find the python executable
pub enum PythonLocation {
    /// Use system interpreter
    System,
    // Use custom interpreter
    Custom(PathBuf),
}

impl PythonLocation {
    /// Location of python executable
    pub fn executable(&self) -> Result<PathBuf, FindPythonError> {
        match self {
            PythonLocation::System => python_executable(),
            PythonLocation::Custom(path) => Ok(path.clone()),
        }
    }
}

/// Create a virtual environment at specified directory
pub fn create_venv(venv_dir: &Path, python: PythonLocation) -> Result<VEnv, VEnvError> {
    let python = python.executable()?;
    let mut cmd = Command::new(python);
    cmd.arg("-m");
    cmd.arg("venv");
    cmd.arg(venv_dir);
    // Don't need pip for our use-case
    cmd.arg("--without-pip");

    let output = cmd.output()?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stderr);
        return Err(VEnvError::FailedToRun(stdout.to_string()));
    }
    Ok(VEnv::new(venv_dir.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use crate::venv::PythonLocation;

    #[test]
    pub fn venv_creation() {
        let venv_dir = tempfile::tempdir().unwrap();
        let venv = super::create_venv(venv_dir.path(), PythonLocation::System).unwrap();
        assert!(venv.location.join("bin/python").is_file());
    }
}
