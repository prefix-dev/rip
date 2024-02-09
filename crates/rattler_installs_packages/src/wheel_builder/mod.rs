//! Turn an sdist into a wheel by creating a virtualenv and building the sdist in it

mod build_environment;
mod error;
mod wheel_cache;

use fs_err as fs;

use std::collections::HashSet;
use std::str::FromStr;

use std::sync::Arc;
use std::{collections::HashMap, path::PathBuf};

use parking_lot::Mutex;
use pep508_rs::MarkerEnvironment;

use crate::python_env::{ParsePythonInterpreterVersionError, PythonInterpreterVersion};
use crate::resolve::solve_options::{OnWheelBuildFailure, ResolveOptions};
use crate::types::ArtifactFromSource;
use crate::types::{NormalizedPackageName, PackageName, SourceArtifactName, WheelFilename};
use crate::wheel_builder::build_environment::BuildEnvironment;
pub use crate::wheel_builder::wheel_cache::{WheelCache, WheelCacheKey, ProjectInfoCache, ProjectInfoCacheKey};
use crate::{artifacts::Wheel, index::PackageDb, python_env::WheelTags, types::WheelCoreMetadata};
pub use error::WheelBuildError;

type BuildCache = Mutex<HashMap<SourceArtifactName, Arc<BuildEnvironment>>>;

/// A builder for wheels
pub struct WheelBuilder {
    /// A cache for virtualenvs that might be reused later in the process
    venv_cache: BuildCache,

    /// The package database to use
    package_db: Arc<PackageDb>,

    /// The env markers to use when resolving
    env_markers: Arc<MarkerEnvironment>,

    /// The configured wheel tags to use when resolving
    wheel_tags: Option<Arc<WheelTags>>,

    /// The resolve options. Note that we change the sdist resolution to normal if it's set to
    /// only sdists, because otherwise we run into a chicken & egg problem where a sdist is required
    /// to build a sdist. E.g. `hatchling` requires `hatchling` as build system.
    resolve_options: ResolveOptions,

    /// The passed environment variables
    env_variables: HashMap<String, String>,

    /// Saved build environments
    /// This is used to save build environments for debugging
    /// only if the `save_on_failure` option is set in resolve options
    saved_build_envs: Mutex<HashSet<PathBuf>>,

    /// Python interpreter version
    python_version: PythonInterpreterVersion,
}

impl WheelBuilder {
    /// Create a new wheel builder
    pub fn new(
        package_db: Arc<PackageDb>,
        env_markers: Arc<MarkerEnvironment>,
        wheel_tags: Option<Arc<WheelTags>>,
        resolve_options: ResolveOptions,
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
            saved_build_envs: Mutex::new(HashSet::new()),
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
        sdist: &impl ArtifactFromSource,
    ) -> Result<Arc<BuildEnvironment>, WheelBuildError> {
        if let Some(venv) = self.venv_cache.lock().get(&sdist.artifact_name()) {
            tracing::debug!(
                "using cached virtual env for: {:?}",
                sdist.distribution_name()
            );
            return Ok(venv.clone());
        }

        tracing::debug!("creating virtual env for: {:?}", sdist.distribution_name());

        let mut build_environment = BuildEnvironment::setup(sdist, self).await?;

        build_environment.install_build_files(sdist)?;

        // Install extra requirements if any
        build_environment.install_extra_requirements(self).await?;

        // Insert into the venv cache
        self.venv_cache
            .lock()
            .insert(sdist.artifact_name().clone(), Arc::new(build_environment));

        // Return the cached values
        return self
            .venv_cache
            .lock()
            .get(&sdist.artifact_name())
            .cloned()
            .ok_or_else(|| WheelBuildError::Error("Could not get venv from cache".to_string()));
    }

    /// Get the paths to the saved build environments
    pub fn saved_build_envs(&self) -> HashSet<PathBuf> {
        self.saved_build_envs.lock().clone()
    }

    /// Handle's a build failure by either saving the build environment or deleting it
    fn handle_build_failure<T>(
        &self,
        result: Result<T, WheelBuildError>,
        build_environment: &BuildEnvironment,
    ) -> Result<T, WheelBuildError> {
        if self.resolve_options.on_wheel_build_failure != OnWheelBuildFailure::SaveBuildEnv {
            return result;
        }
        if let Err(e) = result {
            // Persist the build environment
            build_environment.persist();

            // Save the information for later usage
            let path = build_environment.work_dir();
            tracing::info!("saved build environment is available at: {:?}", &path);
            self.saved_build_envs
                .lock()
                .insert(build_environment.work_dir());
            Err(e)
        } else {
            result
        }
    }

    /// Get the metadata for a given sdist by using the build_backend in a virtual env
    /// This function uses the `prepare_metadata_for_build_wheel` entry point of the build backend.

    #[tracing::instrument(skip_all, fields(name = %sdist.distribution_name(), version = %sdist.version()))]
    pub async fn get_sdist_metadata<S: ArtifactFromSource>(
        &self,
        sdist: &S,
    ) -> Result<(Vec<u8>, WheelCoreMetadata), WheelBuildError> {
        // See if we have a locally built wheel for this sdist
        // use that metadata instead
        let key: WheelCacheKey = WheelCacheKey::from_sdist(sdist, &self.python_version)?;
        if let Some(wheel) = self.package_db.local_wheel_cache().wheel_for_key(&key)? {
            return wheel.metadata().map_err(|e| {
                WheelBuildError::Error(format!("Could not parse wheel metadata: {}", e))
            });
        }

        let build_environment = self.setup_build_venv(sdist).await?;

        // Capture the result of the build
        // to handle different failure modes
        let result = self
            .get_sdist_metadata_internal(&build_environment, sdist)
            .await;
        self.handle_build_failure(result, &build_environment)
    }

    async fn get_sdist_metadata_internal<S: ArtifactFromSource>(
        &self,
        build_environment: &BuildEnvironment,
        sdist: &S,
    ) -> Result<(Vec<u8>, WheelCoreMetadata), WheelBuildError> {
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
    #[tracing::instrument(skip_all, fields(name = %sdist.distribution_name(), version = %sdist.version()))]
    pub async fn build_wheel<S: ArtifactFromSource>(
        &self,
        sdist: &S,
    ) -> Result<Wheel, WheelBuildError> {
        // Check if we have already built this wheel locally and use that instead
        let key = WheelCacheKey::from_sdist(sdist, &self.python_version)?;
        if let Some(wheel) = self.package_db.local_wheel_cache().wheel_for_key(&key)? {
            return Ok(wheel);
        }

        // Setup a new virtualenv for building the wheel or use an existing
        let build_environment = self.setup_build_venv(sdist).await?;
        // Capture the result of the build
        // to handle different failure modes
        let result = self.build_wheel_internal(&build_environment, sdist).await;

        self.handle_build_failure(result, &build_environment)
    }

    async fn build_wheel_internal<S: ArtifactFromSource>(
        &self,
        build_environment: &BuildEnvironment,
        sdist: &S,
    ) -> Result<Wheel, WheelBuildError> {
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
        let package_name: NormalizedPackageName = PackageName::from_str(&sdist.distribution_name())
            .unwrap()
            .into();

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
    use crate::index::{PackageDb, PackageSourcesBuilder};
    use crate::python_env::{Pep508EnvMakers, PythonInterpreterVersion};
    use crate::resolve::solve_options::ResolveOptions;
    use crate::wheel_builder::wheel_cache::WheelCacheKey;
    use crate::wheel_builder::WheelBuilder;
    use futures::future::TryJoinAll;
    use reqwest::Client;
    use reqwest_middleware::ClientWithMiddleware;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn get_package_db() -> (Arc<PackageDb>, TempDir) {
        let tempdir = tempfile::tempdir().unwrap();
        let client = ClientWithMiddleware::from(Client::new());

        let url = url::Url::parse("https://pypi.org/simple/").unwrap();
        let sources = PackageSourcesBuilder::new(url).build().unwrap();

        (
            Arc::new(PackageDb::new(sources, client, tempdir.path()).unwrap()),
            tempdir,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_with_cache() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
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

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_wheel_and_save_env() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/tampered-rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"tampered-rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let resolve_options = ResolveOptions {
            on_wheel_build_failure:
                crate::resolve::solve_options::OnWheelBuildFailure::SaveBuildEnv,
            ..Default::default()
        };

        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            resolve_options,
            Default::default(),
        )
        .unwrap();

        // Build the wheel
        // this should fail because we don't have the right environment
        let result = wheel_builder.build_wheel(&sdist).await;
        assert!(result.is_err());

        let saved_build_envs = wheel_builder.saved_build_envs();
        assert_eq!(saved_build_envs.len(), 1);

        let path = saved_build_envs.iter().next().unwrap();

        // Check if the build env is there
        assert!(path.exists());
    }

    // Skipped for now will fix this in a later PR
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    pub async fn build_sdist_metadata_concurrently() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);

        let wheel_builder = Arc::new(
            WheelBuilder::new(
                package_db.0,
                env_markers,
                None,
                ResolveOptions::default(),
                Default::default(),
            )
            .unwrap(),
        );

        let mut handles = vec![];

        for _ in 0..10 {
            let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();
            let wheel_builder = wheel_builder.clone();
            handles.push(tokio::spawn(async move {
                wheel_builder.get_sdist_metadata(&sdist).await
            }));
        }

        let result = handles.into_iter().collect::<TryJoinAll<_>>().await;
        match result {
            Ok(results) => {
                for result in results {
                    assert!(
                        result.is_ok(),
                        "error during concurrent wheel build: {:?}",
                        result.err()
                    );
                }
            }
            Err(e) => {
                panic!("Failed to build wheels concurrently: {}", e);
            }
        }
    }
}
