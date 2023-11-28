use super::Pep508EnvMakers;
use crate::python_env::{system_python_executable, FindPythonError};
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::process::ExitStatus;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FromPythonError {
    #[error(transparent)]
    CouldNotFindPythonExecutable(#[from] FindPythonError),

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
        let python = system_python_executable()?;
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
                return Err(FromPythonError::CouldNotFindPythonExecutable(
                    FindPythonError::NotFound,
                ))
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

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    pub async fn test_from_env() {
        match Pep508EnvMakers::from_env().await {
            Err(FromPythonError::CouldNotFindPythonExecutable(_)) => {
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
