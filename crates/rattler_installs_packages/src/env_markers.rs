use crate::marker::Env;
use serde::{Deserialize, Serialize};
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::process::ExitStatus;
use thiserror::Error;

/// Describes the environment markers that can be used in dependency specifications to enable or
/// disable certain dependencies based on runtime environment.
///
/// Exactly the markers defined in this struct must be present during version resolution. Unknown
/// variables should raise an error.
///
/// Note that the "extra" variable is not defined in this struct because it depends on the wheel
/// that is being inspected.
///
/// The behavior and the names of the markers are described in PEP 508.
#[derive(Default, Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Pep508EnvMakers {
    pub os_name: String,
    pub sys_platform: String,
    pub platform_machine: String,
    pub platform_python_implementation: String,
    pub platform_release: String,
    pub platform_system: String,
    pub platform_version: String,
    pub python_version: String,
    pub python_full_version: String,
    pub implementation_name: String,
    pub implementation_version: String,
}

#[derive(Debug, Error)]
pub enum FromPythonError {
    #[error("could not find python executable")]
    CouldNotFindPythonExecutable,

    #[error(transparent)]
    FailedToExecute(#[from] io::Error),

    #[error(transparent)]
    FailedToParse(#[from] serde_json::Error),

    #[error("execution failed with exit code {0}")]
    FailedToRun(ExitStatus),
}

impl Pep508EnvMakers {
    /// Try to determine the environment markers by executing python.
    pub async fn from_env() -> Result<Self, FromPythonError> {
        Self::from_python(Path::new("python")).await
    }

    /// Try to determine the environment markers from an existing python executable. The executable
    /// is used to run a simple python program to extract the information.
    pub async fn from_python(python: &Path) -> Result<Self, FromPythonError> {
        let pep508_bytes = include_str!("pep508.py");

        // Execute the python executable
        let output = match tokio::process::Command::new(python)
            .arg("-c")
            .arg(pep508_bytes)
            .output()
            .await
        {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(FromPythonError::CouldNotFindPythonExecutable)
            }
            Err(e) => return Err(FromPythonError::FailedToExecute(e)),
            Ok(output) => output,
        };

        // Ensure that we have a valid success code
        if !output.status.success() {
            return Err(FromPythonError::FailedToRun(output.status));
        }

        // Convert the JSON
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(serde_json::from_str(stdout.trim())?)
    }
}

impl Env for Pep508EnvMakers {
    fn get_marker_var(&self, var: &str) -> Option<&str> {
        match var {
            "os_name" => Some(&self.os_name),
            "sys_platform" => Some(&self.sys_platform),
            "platform_machine" => Some(&self.platform_machine),
            "platform_python_implementation" => Some(&self.platform_python_implementation),
            "platform_release" => Some(&self.platform_release),
            "platform_system" => Some(&self.platform_system),
            "platform_version" => Some(&self.platform_version),
            "python_version" => Some(&self.python_version),
            "python_full_version" => Some(&self.python_full_version),
            "implementation_name" => Some(&self.implementation_name),
            "implementation_version" => Some(&self.implementation_version),
            _ => None,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    pub async fn test_from_env() {
        match Pep508EnvMakers::from_env().await {
            Err(FromPythonError::CouldNotFindPythonExecutable) => {
                // This is fine, the test machine does not include a python binary.
            }
            Err(e) => panic!("{e}"),
            Ok(env) => {
                println!(
                    "Found the following environment markers on the current system:\n\n{env:#?}"
                )
            }
        }
    }
}
