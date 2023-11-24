//! Module that helps with allowing in the creation of python virtual environments.
//! Now just use the python venv command to create the virtual environment.
//! Later on we can look into actually creating the environment by linking to the python library,
//! and creating the necessary files. See: [VEnv](https://packaging.python.org/en/latest/specifications/virtual-environments/#declaring-installation-environments-as-python-virtual-environments)
#![allow(dead_code)]
use crate::system_python::{
    system_python_executable, FindPythonError, ParsePythonInterpreterVersionError,
    PythonInterpreterVersion,
};
use crate::wheel::{UnpackError, UnpackedWheel};
use crate::{InstallPaths, UnpackWheelOptions, Wheel};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use thiserror::Error;

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
            PythonLocation::System => system_python_executable(),
            PythonLocation::Custom(path) => Ok(path.clone()),
        }
    }
}

#[derive(Error, Debug)]
pub enum VEnvError {
    #[error(transparent)]
    FindPythonError(#[from] FindPythonError),
    #[error(transparent)]
    ParsePythonInterpreterVersionError(#[from] ParsePythonInterpreterVersionError),
    #[error("failed to run 'python -m venv': `{0}`")]
    FailedToRun(String),
    #[error(transparent)]
    FailedToCreate(#[from] std::io::Error),
}

/// Represents a virtual environment in which wheels can be installed
pub struct VEnv {
    /// Location of the virtual environment
    location: PathBuf,
    /// Install paths for this virtual environment
    install_paths: InstallPaths,
}

impl VEnv {
    fn new(location: PathBuf, install_paths: InstallPaths) -> Self {
        Self {
            location,
            install_paths,
        }
    }

    /// Install a wheel into this virtual environment
    pub fn install_wheel(
        &self,
        wheel: &Wheel,
        options: &UnpackWheelOptions,
    ) -> Result<UnpackedWheel, UnpackError> {
        wheel.unpack(&self.location, &self.install_paths, options)
    }

    /// Execute python script in venv
    pub fn execute_script(&self, source: &Path) -> std::io::Result<Output> {
        let mut cmd = Command::new(self.python_executable());
        cmd.arg(source);
        cmd.output()
    }

    /// Execute python command in venv
    pub fn execute_command<S: AsRef<str>>(&self, command: S) -> std::io::Result<Output> {
        let mut cmd = Command::new(self.python_executable());
        cmd.arg("-c");
        cmd.arg(command.as_ref());
        cmd.output()
    }

    /// Path to python executable in venv
    pub fn python_executable(&self) -> PathBuf {
        let executable = if self.install_paths.is_windows() {
            "python.exe"
        } else {
            "python"
        };
        self.location
            .join(self.install_paths.scripts())
            .join(executable)
    }

    /// Create a virtual environment at specified directory
    /// for the platform we are running on
    pub fn create(venv_dir: &Path, python: PythonLocation) -> Result<VEnv, VEnvError> {
        Self::create_custom(venv_dir, python, cfg!(windows))
    }

    /// Create a virtual environment at specified directory
    /// allows specifying if this is a windows venv
    pub fn create_custom(
        venv_dir: &Path,
        python: PythonLocation,
        windows: bool,
    ) -> Result<VEnv, VEnvError> {
        // Find python executable
        let python = python.executable()?;

        // Execute command
        // Don't need pip for our use-case
        let output = Command::new(&python)
            .arg("-m")
            .arg("venv")
            .arg(venv_dir)
            .arg("--without-pip")
            .output()?;

        // Parse output
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(VEnvError::FailedToRun(stdout.to_string()));
        }

        let version = PythonInterpreterVersion::from_path(&python)?;
        let install_paths = InstallPaths::for_venv(version, windows);
        Ok(VEnv::new(venv_dir.to_path_buf(), install_paths))
    }
}

#[cfg(test)]
mod tests {
    use super::VEnv;
    use crate::venv::PythonLocation;
    use crate::NormalizedPackageName;
    use std::path::Path;
    use std::str::FromStr;

    #[test]
    pub fn venv_creation() {
        let venv_dir = tempfile::tempdir().unwrap();
        let venv = VEnv::create(venv_dir.path(), PythonLocation::System).unwrap();
        // Does python exist
        assert!(venv.python_executable().is_file());

        // Install wheel
        let wheel = crate::wheel::Wheel::from_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../test-data/wheels/wordle_python-2.3.32-py3-none-any.whl"),
            &NormalizedPackageName::from_str("wordle_python").unwrap(),
        )
        .unwrap();
        venv.install_wheel(&wheel, &Default::default()).unwrap();

        // See if it worked
        let output = venv
            .execute_script(
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../test-data/scripts/test_wordle.py"),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "('A   d   i   E   u   ', False)"
        );
    }
}
