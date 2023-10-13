use pep508_rs::MarkerEnvironment;
use serde::{Deserialize, Serialize};
use std::io;
use std::io::ErrorKind;
use std::ops::Deref;
use std::path::Path;
use std::process::ExitStatus;
use thiserror::Error;
use which::which;

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
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[allow(missing_docs)]
#[serde(transparent)]
pub struct Pep508EnvMakers(pub pep508_rs::MarkerEnvironment);

impl From<pep508_rs::MarkerEnvironment> for Pep508EnvMakers {
    fn from(value: pep508_rs::MarkerEnvironment) -> Self {
        Self(value)
    }
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
        let python = which("python").map_err(|_| FromPythonError::CouldNotFindPythonExecutable)?;
        tracing::info!("using python executable at {}", python.display());
        Self::from_python(python.as_path()).await
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

impl Deref for Pep508EnvMakers {
    type Target = MarkerEnvironment;

    fn deref(&self) -> &Self::Target {
        &self.0
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
