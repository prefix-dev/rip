use crate::install::UnpackError;
use crate::python_env::VEnvError;
use crate::types::{ParseArtifactNameError, WheelCoreMetaDataError};
use crate::wheel_builder::wheel_cache;
use pep508_rs::Requirement;
use std::path::PathBuf;

/// An error that can occur while building a wheel
#[allow(missing_docs)]
#[derive(thiserror::Error, Debug)]
pub enum WheelBuildError {
    #[error("could not build wheel: {0}")]
    Error(String),

    #[error("could not install artifact in virtual environment: {0}")]
    UnpackError(#[from] UnpackError),

    #[error("could not build wheel: {0}")]
    IoError(#[from] std::io::Error),

    #[error("could not run command {0} to build wheel: {1}")]
    CouldNotRunCommand(String, std::io::Error),

    #[error("could not resolve environment for wheel building: {1:?}")]
    CouldNotResolveEnvironment(Vec<Requirement>, miette::Report),

    #[error("error parsing JSON from extra_requirements.json: {0}")]
    JSONError(#[from] serde_json::Error),

    #[error("could not parse generated wheel metadata: {0}")]
    WheelCoreMetadataError(#[from] WheelCoreMetaDataError),

    #[error("could not get artifact: {0}")]
    CouldNotGetArtifact(miette::Report),

    #[error("could not get artifact from cache: {0}")]
    CacheError(#[from] wheel_cache::WheelCacheError),

    #[error("error parsing artifact name: {0}")]
    ArtifactError(#[from] ParseArtifactNameError),

    #[error("error creating venv: {0}")]
    VEnvError(#[from] VEnvError),

    #[error("backend path in pyproject.toml not relative: {0}")]
    BackendPathNotRelative(PathBuf),

    #[error(
        "backend path in pyproject.toml not resolving to a path in the package directory: {0}"
    )]
    BackendPathNotInPackageDir(PathBuf),

    #[error("could not join path: {0}")]
    CouldNotJoinPath(#[from] std::env::JoinPathsError),
}
