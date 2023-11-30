//! Turn an sdist into a wheel by creating a virtualenv and building the sdist in it

mod build_environment;

use parking_lot::Mutex;
use std::sync::Arc;
use std::{collections::HashMap, path::PathBuf};

use pep508_rs::{MarkerEnvironment, Requirement};

use crate::{
    artifacts::wheel::UnpackError,
    artifacts::SDist,
    artifacts::Wheel,
    index::PackageDb,
    python_env::WheelTags,
    types::Artifact,
    types::SDistFilename,
    types::{WheelCoreMetaDataError, WheelCoreMetadata},
};

use crate::resolve::{ResolveOptions, SDistResolution};
use crate::wheel_builder::build_environment::BuildEnvironment;

type BuildCache<'db> = Mutex<HashMap<SDistFilename, Arc<BuildEnvironment<'db>>>>;

/// A builder for wheels
pub struct WheelBuilder<'db, 'i> {
    /// A cache for virtualenvs that might be reused later in the process
    venv_cache: BuildCache<'db>,

    /// The package database to use
    package_db: &'db PackageDb,

    /// The env markers to use when resolving
    env_markers: &'i MarkerEnvironment,

    /// The configured wheel tags to use when resolving
    wheel_tags: Option<&'i WheelTags>,

    /// The resolve options. Note that we change the sdist resolution to normal if it's set to
    /// only sdists, because otherwise we run into a chicken & egg problem where a sdist is required
    /// to build a sdist. E.g. `hatchling` requires `hatchling` as build system.
    resolve_options: ResolveOptions,
}

/// An error that can occur while building a wheel
#[allow(missing_docs)]
#[derive(thiserror::Error, Debug)]
pub enum WheelBuildError {
    #[error("Could not build wheel: {0}")]
    Error(String),

    #[error("Could not install artifact in virtual environment")]
    UnpackError(#[from] UnpackError),

    #[error("Could not build wheel: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Could not run command {0} to build wheel: {1}")]
    CouldNotRunCommand(String, std::io::Error),

    #[error("Could not resolve environment for wheel building")]
    CouldNotResolveEnvironment(Vec<Requirement>),

    #[error("Error parsing JSON from extra_requirements.json: {0}")]
    JSONError(#[from] serde_json::Error),

    #[error("Could not parse generated wheel metadata: {0}")]
    WheelCoreMetadataError(#[from] WheelCoreMetaDataError),

    #[error("Could not get artifact")]
    CouldNotGetArtifact,
}

/// Get the requirements for the build system from the pyproject.toml
/// will use a default if there are no requirements specified
fn build_requirements(build_system: &pyproject_toml::BuildSystem) -> Vec<Requirement> {
    const DEFAULT_REQUIREMENTS: &[&str; 2] = &["setuptools", "wheel"];
    if build_system.requires.is_empty() {
        DEFAULT_REQUIREMENTS
            .iter()
            .map(|r| Requirement {
                name: r.to_string(),
                extras: None,
                version_or_url: None,
                marker: None,
            })
            .collect()
    } else {
        build_system.requires.clone()
    }
}

impl<'db, 'i> WheelBuilder<'db, 'i> {
    /// Create a new wheel builder
    #[must_use]
    pub fn new(
        package_db: &'db PackageDb,
        env_markers: &'i MarkerEnvironment,
        wheel_tags: Option<&'i WheelTags>,
        _resolve_options: &'i ResolveOptions,
    ) -> Self {
        // TODO: add this back later when we have a wheel cache
        // We are running into a chicken & egg problem if we want to build wheels for packages that
        // require their build system as sdist as well. For example, `hatchling` requires `hatchling` as
        // build system. Hypothetically we'd have to look through all the hatchling sdists to find the one
        // that doesn't depend on itself.
        // Instead, we use wheels to build wheels.
        // let resolve_options = if resolve_options.sdist_resolution == SDistResolution::OnlySDists {
        //     ResolveOptions {
        //         sdist_resolution: SDistResolution::Only,
        //     }
        // } else {
        //     resolve_options.clone()
        // };
        let resolve_options = ResolveOptions {
            sdist_resolution: SDistResolution::OnlyWheels,
        };

        Self {
            venv_cache: Mutex::new(HashMap::new()),
            package_db,
            env_markers,
            wheel_tags,
            resolve_options,
        }
    }

    /// Get a prepared virtualenv for building a wheel (or extracting metadata) from an `[SDist]`
    /// This function also caches the virtualenvs, so that they can be reused later.
    async fn setup_build_venv(
        &self,
        sdist: &SDist,
    ) -> Result<Arc<BuildEnvironment>, WheelBuildError> {
        if let Some(venv) = self.venv_cache.lock().get(sdist.name()) {
            tracing::debug!(
                "using cached virtual env for: {:?}",
                sdist.name().distribution.as_source_str()
            );
            return Ok(venv.clone());
        }

        // If not in cache, create a new one
        tracing::debug!(
            "creating virtual env for: {:?}",
            sdist.name().distribution.as_source_str()
        );

        let build_environment = BuildEnvironment::setup(
            sdist,
            self.package_db,
            self.env_markers,
            self.wheel_tags,
            &self.resolve_options,
        )
        .await?;

        build_environment.install_build_files(sdist)?;

        // Install extra requirements if any
        build_environment
            .install_extra_requirements(
                self.package_db,
                self.env_markers,
                self.wheel_tags,
                &self.resolve_options,
            )
            .await?;

        // Insert into the venv cache
        self.venv_cache
            .lock()
            .insert(sdist.name().clone(), Arc::new(build_environment));

        // Return the cached values
        return self
            .venv_cache
            .lock()
            .get(sdist.name())
            .cloned()
            .ok_or_else(|| WheelBuildError::Error("Could not get venv from cache".to_string()));
    }

    /// Get the metadata for a given sdist by using the build_backend in a virtual env
    /// This function uses the `prepare_metadata_for_build_wheel` entry point of the build backend.
    #[tracing::instrument(skip_all, fields(name = %sdist.name().distribution.as_source_str(), version = %sdist.name().version))]
    pub async fn get_sdist_metadata(
        &self,
        sdist: &SDist,
    ) -> Result<(Vec<u8>, WheelCoreMetadata), WheelBuildError> {
        let build_environment = self.setup_build_venv(sdist).await?;

        let output = build_environment.run_command("WheelMetadata")?;

        if !output.status.success() {
            if output.status.code() == Some(50) {
                tracing::warn!("SDist build backend does not support metadata generation");
                // build wheel instead
                let wheel_file = self.build_wheel(sdist).await?;
                let wheel =
                    Wheel::from_path(&wheel_file, &sdist.name().distribution.clone().into())
                        .map_err(|e| {
                            WheelBuildError::Error(format!(
                                "Could not build wheel for metadata extraction: {}",
                                e
                            ))
                        })?;

                return wheel.metadata().map_err(|e| {
                    WheelBuildError::Error(format!("Could not parse wheel metadata: {}", e))
                });
            }
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(stdout.to_string()));
        }

        let result = std::fs::read_to_string(build_environment.work_dir().join("metadata_result"))?;
        let folder = PathBuf::from(result.trim());
        let path = folder.join("METADATA");

        let metadata = std::fs::read(path)?;
        let wheel_metadata = WheelCoreMetadata::try_from(metadata.as_slice())?;
        Ok((metadata, wheel_metadata))
    }

    /// Build a wheel from an sdist by using the build_backend in a virtual env.
    /// This function uses the `build_wheel` entry point of the build backend.
    #[tracing::instrument(skip_all, fields(name = %sdist.name().distribution.as_source_str(), version = %sdist.name().version))]
    pub async fn build_wheel(&self, sdist: &SDist) -> Result<PathBuf, WheelBuildError> {
        let build_environment = self.setup_build_venv(sdist).await?;

        let output = build_environment.run_command("Wheel")?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(stdout.to_string()));
        }

        let result = std::fs::read_to_string(build_environment.work_dir().join("wheel_result"))?;
        let wheel_file = PathBuf::from(result.trim());

        Ok(wheel_file)
    }
}
