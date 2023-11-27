//! Turn an sdist into a wheel by creating a virtualenv and building the sdist in it
use parking_lot::Mutex;
use std::sync::Arc;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::{Command, Output},
    str::FromStr,
};

use pep508_rs::{MarkerEnvironment, Requirement};

use crate::{
    core_metadata::{WheelCoreMetaDataError, WheelCoreMetadata},
    resolve,
    sdist::SDist,
    tags::WheelTags,
    venv::{PythonLocation, VEnv},
    wheel::UnpackError,
    Artifact, PackageDb, PinnedPackage, ResolveOptions, SDistFilename, SDistResolution,
    UnpackWheelOptions, Wheel,
};

// include static build_frontend.py string
const BUILD_FRONTEND_PY: &str = include_str!("./wheel_builder_frontend.py");

#[derive(Debug)]

/// A build environment for building wheels
/// This struct contains the virtualenv and everything that is needed
/// to execute the PEP517 build backend hools
pub struct BuildEnvironment<'db> {
    work_dir: tempfile::TempDir,
    package_dir: PathBuf,
    #[allow(dead_code)]
    build_system: pyproject_toml::BuildSystem,
    entry_point: String,
    build_requirements: Vec<Requirement>,
    resolved_wheels: Vec<PinnedPackage<'db>>,
    venv: VEnv,
}

impl<'db> BuildEnvironment<'db> {
    /// Extract the wheel and write the build_frontend.py to the work folder
    pub fn install_build_files(&self, sdist: &SDist) -> std::io::Result<()> {
        // Extract the sdist to the work folder
        sdist.extract_to(self.work_dir.path())?;
        // Write the python frontend to the work folder
        std::fs::write(
            self.work_dir.path().join("build_frontend.py"),
            BUILD_FRONTEND_PY,
        )
    }

    /// Get the extra requirements and combine these to the existing requirements
    /// This uses the `GetRequiresForBuildWheel` entry point of the build backend.
    /// this might not be available for all build backends.
    /// and it can also return an empty list of requirements.
    fn get_extra_requirements(&self) -> Result<HashSet<Requirement>, WheelBuildError> {
        let output = self.run_command("GetRequiresForBuildWheel").map_err(|e| {
            WheelBuildError::CouldNotRunCommand("GetRequiresForBuildWheel".into(), e)
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(stderr.to_string()));
        }

        // The extra requirements are stored in a file called extra_requirements.json
        let extra_requirements_json =
            std::fs::read_to_string(self.work_dir.path().join("extra_requirements.json"))?;
        let extra_requirements: Vec<String> = serde_json::from_str(&extra_requirements_json)?;

        Ok(HashSet::<Requirement>::from_iter(
            extra_requirements
                .iter()
                .map(|s| Requirement::from_str(s).expect("...")),
        ))
    }

    /// Install extra requirements into the venv, if any extra were found
    /// If the extra requirements are already installed, this will do nothing
    /// for that requirement.
    async fn install_extra_requirements(
        &self,
        package_db: &'db PackageDb,
        env_markers: &MarkerEnvironment,
        wheel_tags: Option<&WheelTags>,
        resolve_options: &ResolveOptions,
    ) -> Result<(), WheelBuildError> {
        // Get extra requirements if any
        let extra_requirements = self.get_extra_requirements()?;

        // Combine previous requirements with extra requirements
        let combined_requirements = HashSet::from_iter(self.build_requirements.iter().cloned())
            .union(&extra_requirements)
            .cloned()
            .collect::<Vec<_>>();

        // Install extra requirements if any new ones were foujnd
        if !extra_requirements.is_empty()
            && self.build_requirements.len() != combined_requirements.len()
        {
            let locked_packages = HashMap::default();
            // Todo: use the previous resolve for the favored packages?
            let favored_packages = HashMap::default();
            let all_requirements = combined_requirements.to_vec();
            let extra_resolved_wheels = resolve(
                package_db,
                all_requirements.iter(),
                env_markers,
                wheel_tags,
                locked_packages,
                favored_packages,
                resolve_options,
            )
            .await
            .map_err(|_| WheelBuildError::CouldNotResolveEnvironment(all_requirements))?;

            // install extra wheels
            for package_info in extra_resolved_wheels {
                if self.resolved_wheels.contains(&package_info) {
                    continue;
                }
                tracing::info!(
                    "installing extra requirements: {} - {}",
                    package_info.name,
                    package_info.version
                );
                let artifact_info = package_info.artifacts.first().unwrap();
                let artifact = package_db
                    .get_artifact::<Wheel>(artifact_info)
                    .await
                    .expect("could not get artifact");

                self.venv
                    .install_wheel(&artifact, &UnpackWheelOptions::default())?;
            }
        }
        Ok(())
    }

    /// Run a command in the build environment
    fn run_command(&self, stage: &str) -> std::io::Result<Output> {
        // three args: cache.folder, goal
        Command::new(self.venv.python_executable())
            .current_dir(&self.package_dir)
            .arg(self.work_dir.path().join("build_frontend.py"))
            .arg(self.work_dir.path())
            .arg(&self.entry_point)
            .arg(stage)
            .output()
    }

    /// Setup the build environment so that we can build a wheel from an sdist
    async fn setup(
        sdist: &SDist,
        package_db: &'db PackageDb,
        env_markers: &MarkerEnvironment,
        wheel_tags: Option<&WheelTags>,
        resolve_options: &ResolveOptions,
    ) -> Result<BuildEnvironment<'db>, WheelBuildError> {
        // Setup a work directory and a new env dir
        let work_dir = tempfile::tempdir().unwrap();
        let venv = VEnv::create(&work_dir.path().join("venv"), PythonLocation::System).unwrap();

        // Find the build system
        let build_system =
            sdist
                .read_build_info()
                .unwrap_or_else(|_| pyproject_toml::BuildSystem {
                    requires: Vec::new(),
                    build_backend: None,
                    backend_path: None,
                });
        // Find the build requirements
        let build_requirements = build_requirements(&build_system);
        // Resolve the build environment
        let resolved_wheels = resolve(
            package_db,
            build_requirements.iter(),
            env_markers,
            wheel_tags,
            HashMap::default(),
            HashMap::default(),
            resolve_options,
        )
        .await
        .map_err(|_| WheelBuildError::CouldNotResolveEnvironment(build_requirements.to_vec()))?;

        // Install into venv
        for package_info in resolved_wheels.iter() {
            let artifact_info = package_info.artifacts.first().unwrap();
            let artifact = package_db
                .get_artifact::<Wheel>(artifact_info)
                .await
                .map_err(|_| WheelBuildError::CouldNotGetArtifact)?;

            venv.install_wheel(&artifact, &UnpackWheelOptions { installer: None })?;
        }

        const DEFAULT_BUILD_BACKEND: &str = "setuptools.build_meta:__legacy__";
        let entry_point = build_system
            .build_backend
            .clone()
            .unwrap_or_else(|| DEFAULT_BUILD_BACKEND.to_string());

        // Package dir for the package we need to build
        let package_dir = work_dir.path().join(format!(
            "{}-{}",
            sdist.name().distribution.as_source_str(),
            sdist.name().version
        ));

        Ok(BuildEnvironment {
            work_dir,
            package_dir,
            build_system,
            build_requirements,
            entry_point,
            resolved_wheels,
            venv,
        })
    }
}

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
                let wheel = crate::wheel::Wheel::from_path(
                    &wheel_file,
                    &sdist.name().distribution.clone().into(),
                )
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

        let result =
            std::fs::read_to_string(build_environment.work_dir.path().join("metadata_result"))?;
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

        let result =
            std::fs::read_to_string(build_environment.work_dir.path().join("wheel_result"))?;
        let wheel_file = PathBuf::from(result.trim());

        Ok(wheel_file)
    }
}
