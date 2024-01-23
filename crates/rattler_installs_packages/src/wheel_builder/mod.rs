//! Turn an sdist into a wheel by creating a virtualenv and building the sdist in it

mod build_environment;
mod wheel_cache;

use fs_err as fs;

use std::sync::Arc;
use std::{collections::HashMap, path::PathBuf};

use parking_lot::Mutex;
use pep508_rs::{MarkerEnvironment, Requirement};

use crate::python_env::{ParsePythonInterpreterVersionError, PythonInterpreterVersion, VEnvError};
use crate::resolve::ResolveOptions;
use crate::types::{NormalizedPackageName, ParseArtifactNameError, WheelFilename};
use crate::wheel_builder::build_environment::BuildEnvironment;
pub use crate::wheel_builder::wheel_cache::{WheelCache, WheelCacheKey};
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

    /// The passed environment variables
    env_variables: HashMap<String, String>,

    /// Python interpreter version
    python_version: PythonInterpreterVersion,
}

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

impl<'db, 'i> WheelBuilder<'db, 'i> {
    /// Create a new wheel builder
    pub fn new(
        package_db: &'db PackageDb,
        env_markers: &'i MarkerEnvironment,
        wheel_tags: Option<&'i WheelTags>,
        resolve_options: &ResolveOptions,
        env_variables: HashMap<String, String>,
    ) -> Result<Self, ParsePythonInterpreterVersionError> {
        let resolve_options = resolve_options.clone();

        let python_version = resolve_options.python_location.version()?;

        Ok(Self {
            venv_cache: Mutex::new(HashMap::new()),
            package_db,
            env_markers,
            wheel_tags,
            resolve_options,
            env_variables,
            python_version,
        })
    }

    /// Get the python interpreter version
    pub fn python_version(&self) -> &PythonInterpreterVersion {
        &self.python_version
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

        tracing::debug!(
            "creating virtual env for: {:?}",
            sdist.name().distribution.as_source_str()
        );

        let build_environment = BuildEnvironment::setup(
            sdist,
            self,
            self.env_markers,
            self.wheel_tags,
            &self.resolve_options,
            self.env_variables.clone(),
        )
        .await?;

        build_environment.install_build_files(sdist)?;

        // Install extra requirements if any
        build_environment
            .install_extra_requirements(
                self,
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
        // See if we have a locally built wheel for this sdist
        // use that metadata instead
        let key = WheelCacheKey::from_sdist(sdist, &self.python_version)?;
        if let Some(wheel) = self.package_db.local_wheel_cache().wheel_for_key(&key)? {
            return wheel.metadata().map_err(|e| {
                WheelBuildError::Error(format!("Could not parse wheel metadata: {}", e))
            });
        }

        let build_environment = self.setup_build_venv(sdist).await?;

        let output = build_environment.run_command("WheelMetadata")?;

        if !output.status.success() {
            if output.status.code() == Some(50) {
                tracing::warn!("SDist build backend does not support metadata generation");
                // build wheel instead
                let wheel = self.build_wheel(sdist).await?;

                return wheel.metadata().map_err(|e| {
                    WheelBuildError::Error(format!("Could not parse wheel metadata: {}", e))
                });
            }
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(stdout.to_string()));
        }

        let result = fs::read_to_string(build_environment.work_dir().join("metadata_result"))?;
        let folder = PathBuf::from(result.trim());
        let path = folder.join("METADATA");

        let metadata = fs::read(path)?;
        let wheel_metadata = WheelCoreMetadata::try_from(metadata.as_slice())?;
        Ok((metadata, wheel_metadata))
    }

    /// Build a wheel from an sdist by using the build_backend in a virtual env.
    /// This function uses the `build_wheel` entry point of the build backend.
    #[tracing::instrument(skip_all, fields(name = %sdist.name().distribution.as_source_str(), version = %sdist.name().version))]
    pub async fn build_wheel(&self, sdist: &SDist) -> Result<Wheel, WheelBuildError> {
        // Check if we have already built this wheel locally and use that instead
        let key = WheelCacheKey::from_sdist(sdist, &self.python_version)?;
        if let Some(wheel) = self.package_db.local_wheel_cache().wheel_for_key(&key)? {
            return Ok(wheel);
        }

        // Setup a new virtualenv for building the wheel or use an existing
        let build_environment = self.setup_build_venv(sdist).await?;

        // Run the wheel stage
        let output = build_environment.run_command("Wheel")?;

        // Check for success
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(stdout.to_string()));
        }

        // This is where the wheel file is located
        let wheel_file: PathBuf =
            fs::read_to_string(build_environment.work_dir().join("wheel_result"))?
                .trim()
                .into();

        // Get the name of the package
        let package_name: NormalizedPackageName = sdist.name().distribution.clone().into();

        // Save the wheel into the cache
        let key = WheelCacheKey::from_sdist(sdist, &self.python_version)?;

        // Reconstruction of the wheel filename
        let file_component = wheel_file
            .file_name()
            .and_then(|f| f.to_str())
            .ok_or_else(|| {
                WheelBuildError::Error(format!(
                    "Could not get extract file component from {}",
                    wheel_file.display()
                ))
            })?;
        let wheel_file_name = WheelFilename::from_filename(file_component, &package_name)?;

        // Associate the wheel with the key which is the hashed sdist
        self.package_db.local_wheel_cache().associate_wheel(
            &key,
            wheel_file_name,
            &mut fs::File::open(&wheel_file)?,
        )?;

        // Reconstruct wheel from the path
        let wheel = Wheel::from_path(&wheel_file, &package_name)
            .map_err(|e| WheelBuildError::Error(format!("Could not build wheel: {}", e)))?;

        Ok(wheel)
    }
}

#[cfg(test)]
mod tests {
    use crate::artifacts::SDist;
    use crate::index::PackageDb;
    use crate::python_env::{Pep508EnvMakers, PythonInterpreterVersion};
    use crate::resolve::ResolveOptions;
    use crate::wheel_builder::wheel_cache::WheelCacheKey;
    use crate::wheel_builder::WheelBuilder;
    use std::path::Path;
    use tempfile::TempDir;

    fn get_package_db() -> (PackageDb, TempDir) {
        let tempdir = tempfile::tempdir().unwrap();
        (
            PackageDb::new(
                Default::default(),
                &[url::Url::parse("https://pypi.org/simple/").unwrap()],
                tempdir.path(),
            )
            .unwrap(),
            tempdir,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_with_cache() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Pep508EnvMakers::from_env().await.unwrap();
        let resolve_options = ResolveOptions::default();
        let wheel_builder = WheelBuilder::new(
            &package_db.0,
            &env_markers,
            None,
            &resolve_options,
            Default::default(),
        )
        .unwrap();

        // Build the wheel
        wheel_builder.build_wheel(&sdist).await.unwrap();

        // See if we can retrieve it from the cache
        let key = WheelCacheKey::from_sdist(&sdist, wheel_builder.python_version()).unwrap();
        wheel_builder
            .package_db
            .local_wheel_cache()
            .wheel_for_key(&key)
            .unwrap()
            .unwrap();

        // No one will be using 1.0.0, I reckon
        let older_python = PythonInterpreterVersion::new(1, 0, 0);
        let key = WheelCacheKey::from_sdist(&sdist, &older_python).unwrap();
        assert!(wheel_builder
            .package_db
            .local_wheel_cache()
            .wheel_for_key(&key)
            .unwrap()
            .is_none());
    }
}
