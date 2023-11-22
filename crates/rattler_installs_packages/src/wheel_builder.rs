//! Turn an sdist into a wheel by creating a virtualenv and building the sdist in it
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::{Command, Output},
    rc::Rc,
    str::FromStr,
};

use pep508_rs::{MarkerEnvironment, Requirement};

#[derive(Debug)]
/// A fake tempdir for debugging (doesn't delete stuff after it's done)
struct FakeTempDir {
    path: PathBuf,
}

impl FakeTempDir {
    /// Create a new fake tempdir
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            path: dir.into_path(),
        }
    }

    /// Get the path of the tempdir
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

use crate::{
    core_metadata::{WheelCoreMetaDataError, WheelCoreMetadata},
    resolve,
    sdist::SDist,
    tags::WheelTags,
    venv::{PythonLocation, VEnv},
    wheel::UnpackError,
    Artifact, PackageDb, ResolveOptions, SDistName, SDistResolution, UnpackWheelOptions, Wheel,
};

#[derive(Debug)]
pub struct CacheValue {
    folder: FakeTempDir,
    package_dir: PathBuf,
    #[allow(dead_code)]
    build_system: pyproject_toml::BuildSystem,
    entry_point: String,
    requirements: Vec<Requirement>,
    venv: VEnv,
}

type BuildCache = RefCell<HashMap<SDistName, Rc<CacheValue>>>;

// include static build_frontend.py string
const BUILD_FRONTEND_PY: &str = include_str!("./wheel_builder_frontend.py");
/// A builder for wheels
pub struct WheelBuilder<'db, 'i> {
    /// A cache for virtualenvs that might be reused later in the process
    venv_cache: BuildCache,

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

impl<'db, 'i> WheelBuilder<'db, 'i> {
    /// Create a new wheel builder
    #[must_use]
    pub fn new(
        package_db: &'db PackageDb,
        env_markers: &'i MarkerEnvironment,
        wheel_tags: Option<&'i WheelTags>,
        resolve_options: &'i ResolveOptions,
    ) -> Self {
        // We are running into a chicken & egg problem if we want to build wheels for packages that
        // require their build system as sdist as well. For example, `hatchling` requires `hatchling` as
        // build system. Hypothetically we'd have to look through all the hatchling sdists to find the one
        // that doesn't depend on itself.
        // Instead, we use wheels to build wheels.
        let resolve_options = if resolve_options.sdist_resolution == SDistResolution::OnlySDists {
            ResolveOptions {
                sdist_resolution: SDistResolution::Normal,
            }
        } else {
            resolve_options.clone()
        };

        Self {
            venv_cache: RefCell::new(HashMap::new()),
            package_db,
            env_markers,
            wheel_tags,
            resolve_options,
        }
    }

    /// Get a prepared virtualenv for building a wheel (or extracting metadata) from an `[SDist]`
    /// This function also caches the virtualenvs, so that they can be reused later.
    async fn get_venv(&self, sdist: &SDist) -> Result<Rc<CacheValue>, WheelBuildError> {
        if let Some(venv) = self.venv_cache.borrow().get(sdist.name()) {
            return Ok(venv.clone());
        }

        // If not in cache, create a new one
        tracing::info!("Creating virtual env for: {:?}", sdist.name());

        // let folder = tempfile::tempdir().unwrap();
        let folder = FakeTempDir::new(); // TODO use proper tempdir here
        let venv = VEnv::create(&folder.path().join("venv"), PythonLocation::System).unwrap();

        let build_system =
            sdist
                .read_build_info()
                .unwrap_or_else(|_| pyproject_toml::BuildSystem {
                    requires: Vec::new(),
                    build_backend: None,
                    backend_path: None,
                });

        const DEFAULT_REQUIREMENTS: &[&str; 2] = &["setuptools", "wheel"];
        let requirements = if build_system.requires.is_empty() {
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
        };

        let locked_packages = HashMap::default();
        let favored_packages = HashMap::default();

        let resolved_wheels = resolve(
            self.package_db,
            requirements.iter(),
            self.env_markers,
            self.wheel_tags,
            locked_packages,
            favored_packages,
            &self.resolve_options,
        )
        .await
        .map_err(|_| WheelBuildError::CouldNotResolveEnvironment(requirements.clone()))?;

        // TODO: what's this?
        let options = UnpackWheelOptions { installer: None };

        for package_info in resolved_wheels.iter() {
            let artifact_info = package_info.artifacts.first().unwrap();
            let artifact = self
                .package_db
                .get_artifact::<Wheel>(artifact_info)
                .await
                .map_err(|_| WheelBuildError::CouldNotGetArtifact)?;

            venv.install_wheel(&artifact, &options)?;
        }

        const DEFAULT_BUILD_BACKEND: &str = "setuptools.build_meta:__legacy__";
        let entry_point = build_system
            .build_backend
            .clone()
            .unwrap_or_else(|| DEFAULT_BUILD_BACKEND.to_string());

        // Extract the sdist to the work folder
        sdist.extract_to(folder.path())?;

        std::fs::write(folder.path().join("build_frontend.py"), BUILD_FRONTEND_PY)?;

        let package_dir = folder.path().join(format!(
            "{}-{}",
            sdist.name().distribution.as_source_str(),
            sdist.name().version
        ));

        let cache_value = CacheValue {
            folder,
            package_dir,
            build_system,
            requirements,
            entry_point,
            venv,
        };

        let output = self
            .run_command(&cache_value, "GetRequiresForBuildWheel")
            .map_err(|e| {
                WheelBuildError::CouldNotRunCommand("GetRequiresForBuildWheel".into(), e)
            })?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(format!(
                "Could not build wheel: {}",
                stdout
            )));
        }

        let extra_requirements_json =
            std::fs::read_to_string(cache_value.folder.path().join("extra_requirements.json"))?;

        let extra_requirements: Vec<String> = serde_json::from_str(&extra_requirements_json)?;

        let requirements =
            HashSet::<Requirement>::from_iter(cache_value.requirements.iter().cloned());
        let extra_requirements = HashSet::<Requirement>::from_iter(
            extra_requirements
                .iter()
                .map(|s| Requirement::from_str(s).expect("...")),
        );
        let combined_requirements = requirements
            .union(&extra_requirements)
            .cloned()
            .collect::<Vec<_>>();

        if !extra_requirements.is_empty() && requirements.len() != combined_requirements.len() {
            let locked_packages = HashMap::default();
            // Todo: use the previous resolve for the favored packages?
            let favored_packages = HashMap::default();
            let all_requirements = combined_requirements.to_vec();
            let extra_resolved_wheels = resolve(
                self.package_db,
                all_requirements.iter(),
                self.env_markers,
                self.wheel_tags,
                locked_packages,
                favored_packages,
                &self.resolve_options,
            )
            .await
            .map_err(|_| WheelBuildError::CouldNotResolveEnvironment(all_requirements))?;

            // install extra wheels
            for package_info in extra_resolved_wheels {
                if resolved_wheels.contains(&package_info) {
                    continue;
                }
                tracing::info!(
                    "Wheel building: installing extra requirements: {} - {}",
                    package_info.name,
                    package_info.version
                );
                let artifact_info = package_info.artifacts.first().unwrap();
                let artifact = self
                    .package_db
                    .get_artifact::<Wheel>(artifact_info)
                    .await
                    .expect("Could not get artifact");

                cache_value.venv.install_wheel(&artifact, &options)?;
            }
        }

        self.venv_cache
            .borrow_mut()
            .insert(sdist.name().clone(), Rc::new(cache_value));

        return self
            .venv_cache
            .borrow()
            .get(sdist.name())
            .cloned()
            .ok_or_else(|| WheelBuildError::Error("Could not get venv from cache".to_string()));
    }

    fn run_command(&self, cache: &CacheValue, stage: &str) -> std::io::Result<Output> {
        // three args: cache.folder, goal
        Command::new(cache.venv.python_executable())
            .current_dir(&cache.package_dir)
            .arg(cache.folder.path().join("build_frontend.py"))
            .arg(cache.folder.path())
            .arg(&cache.entry_point)
            .arg(stage)
            .output()
    }

    /// Get the metadata for a given sdist by using the build_backend in a virtual env
    /// This function uses the `prepare_metadata_for_build_wheel` entry point of the build backend.
    pub async fn get_metadata(
        &self,
        sdist: &SDist,
    ) -> Result<(Vec<u8>, WheelCoreMetadata), WheelBuildError> {
        let cache = self.get_venv(sdist).await?;

        let output = self.run_command(&cache, "WheelMetadata")?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(format!(
                "Could not build wheel: {}",
                stdout
            )));
        }

        let result = std::fs::read_to_string(cache.folder.path().join("metadata_result"))?;
        let folder = PathBuf::from(result.trim());
        let path = folder.join("METADATA");

        let metadata = std::fs::read(path)?;
        let wheel_metadata = WheelCoreMetadata::try_from(metadata.as_slice())?;
        Ok((metadata, wheel_metadata))
    }

    /// Build a wheel from an sdist by using the build_backend in a virtual env.
    /// This function uses the `build_wheel` entry point of the build backend.
    pub async fn build_wheel(&self, sdist: &SDist) -> Result<PathBuf, WheelBuildError> {
        let cache = self.get_venv(sdist).await?;

        let output = self.run_command(&cache, "Wheel")?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(format!(
                "Could not build wheel: {}",
                stdout
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let wheel_file = PathBuf::from(stdout.trim());
        Ok(wheel_file)
    }
}
