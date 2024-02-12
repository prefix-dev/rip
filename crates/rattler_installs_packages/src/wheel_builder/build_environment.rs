use crate::artifacts::wheel::UnpackWheelOptions;
use crate::types::ArtifactFromSource;

use crate::python_env::{PythonLocation, VEnv};
use crate::resolve::{resolve, PinnedPackage};
use crate::utils::normalize_path;
use crate::wheel_builder::{WheelBuildError, WheelBuilder};
use fs_err as fs;
use fs_err::read_dir;
use parking_lot::RwLock;
use pep508_rs::Requirement;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;

use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;

#[derive(Debug)]
enum DeleteOrPersist {
    /// Delete the temp dir when the BuildEnvironment is dropped
    Delete(tempfile::TempDir),
    /// Persist the temp dir
    Persist(PathBuf),
}

impl DeleteOrPersist {
    /// Persist the temp dir
    fn persist(self) -> Self {
        if let Self::Delete(temp_dir) = self {
            // This operation makes sure that the tempdir is not deleted
            // when the BuildEnvironment is dropped
            Self::Persist(temp_dir.into_path())
        } else {
            self
        }
    }

    /// Get the path of the temp dir
    fn path(&self) -> &Path {
        match self {
            Self::Delete(dir) => dir.path(),
            Self::Persist(path) => path,
        }
    }
}

#[derive(Debug)]
struct TempBuildEnvironment {
    delete_or_persist: RwLock<Option<DeleteOrPersist>>,
}

impl TempBuildEnvironment {
    fn new(temp_dir: tempfile::TempDir) -> Self {
        Self {
            delete_or_persist: RwLock::new(Some(DeleteOrPersist::Delete(temp_dir))),
        }
    }

    /// Persist the temporary build environment
    fn persist(&self) {
        let mut delete_or_persist = self.delete_or_persist.write();
        let new_value = delete_or_persist
            .deref_mut()
            .take()
            .expect("inner value: 'delete_or_persist' was already taken, this should never happen")
            .persist();
        *delete_or_persist = Some(new_value);
    }

    /// Path to the temporary build environment
    fn path(&self) -> PathBuf {
        self.delete_or_persist
            .read()
            .as_ref()
            .expect("delete_or_persist should always have a value")
            .path()
            .to_path_buf()
    }
}

// include static build_frontend.py string
const BUILD_FRONTEND_PY: &str = include_str!("./wheel_builder_frontend.py");
/// A build environment for building wheels
/// This struct contains the virtualenv and everything that is needed
/// to execute the PEP517 build backend hools
#[derive(Debug)]
pub(crate) struct BuildEnvironment {
    work_dir: TempBuildEnvironment,
    package_dir: PathBuf,
    #[allow(dead_code)]
    build_system: pyproject_toml::BuildSystem,
    entry_point: String,
    build_requirements: Vec<Requirement>,
    resolved_wheels: Vec<PinnedPackage>,
    venv: VEnv,
    env_variables: HashMap<String, String>,
    clean_env: bool,
    #[allow(dead_code)]
    python_location: PythonLocation,
}

fn normalize_backend_path(
    backend_path: &[String],
    package_dir: &Path,
) -> Result<Vec<PathBuf>, WheelBuildError> {
    let normed = backend_path
        .iter()
        .map(|s| PathBuf::from_str(s).unwrap())
        .map(|p| {
            if p.is_absolute() {
                Err(WheelBuildError::BackendPathNotRelative(p))
            } else {
                Ok(normalize_path(&package_dir.join(p)))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    // for each normed path, make sure that it shares the package_dir as prefix
    for path in normed.iter() {
        if !path.starts_with(package_dir) {
            return Err(WheelBuildError::BackendPathNotInPackageDir(path.clone()));
        }
    }

    Ok(normed)
}

impl BuildEnvironment {
    /// Extract the wheel and write the build_frontend.py to the work folder
    pub(crate) fn install_build_files(
        &mut self,
        sdist: &(impl ArtifactFromSource + ?Sized),
    ) -> std::io::Result<()> {
        // Extract the sdist to the work folder
        // extract to a specific package dir
        let work_dir = self.work_dir.path();

        sdist.extract_to(work_dir.as_path())?;

        // when sdists are downloaded from pypi - they have correct name
        // name - version
        // when we are using direct versions, we don't know the actual version
        // so we create package-dir as name-file://version or name-http://your-url-version
        // which is not actually true
        // so after extracting or moving
        // we map correct package location
        // when URL is actually a git version
        // it is extracted in work_dir
        // so we map package_dir to work_dir

        if sdist.version().is_git() {
            self.package_dir = self.work_dir.path();
        } else if let Some(package_dir_name) = self.package_dir.file_name() {
            let actual_package_dir = work_dir.join(package_dir_name);
            if !actual_package_dir.exists() {
                for path in (read_dir(work_dir.clone())?).flatten() {
                    if path
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.contains(&sdist.distribution_name()))
                    {
                        self.package_dir = path.path();
                        break;
                    }
                }
            }
        }

        // Write the python frontend to the work folder
        fs::write(work_dir.join("build_frontend.py"), BUILD_FRONTEND_PY)
    }

    /// Get the path to the work directory
    /// The work directory is the location of the SDist source code
    /// and python build_frontend.py
    pub(crate) fn work_dir(&self) -> PathBuf {
        self.work_dir.path()
    }

    /// Get the extra requirements and combine these to the existing requirements
    /// This uses the `GetRequiresForBuildWheel` entry point of the build backend.
    /// this might not be available for all build backends.
    /// and it can also return an empty list of requirements.
    fn get_extra_requirements(&self) -> Result<HashSet<Requirement>, WheelBuildError> {
        let output = self.run_command("GetRequiresForBuildWheel")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(stderr.to_string()));
        }

        // The extra requirements are stored in a file called extra_requirements.json
        let extra_requirements_json =
            fs::read_to_string(self.work_dir.path().join("extra_requirements.json"))?;
        let extra_requirements: Vec<String> = serde_json::from_str(&extra_requirements_json)?;

        Ok(HashSet::<Requirement>::from_iter(
            extra_requirements
                .iter()
                .map(|s| Requirement::from_str(s).expect("...")),
        ))
    }

    /// Persist the build environment
    /// Don't delete the work directory if the BuildEnvironment is dropped
    pub fn persist(&self) -> PathBuf {
        self.work_dir.persist();
        self.work_dir.path()
    }

    /// Install extra requirements into the venv, if any extra were found
    /// If the extra requirements are already installed, this will do nothing
    /// for that requirement.
    pub(crate) async fn install_extra_requirements(
        &self,
        wheel_builder: &WheelBuilder,
    ) -> Result<(), WheelBuildError> {
        // Get extra requirements if any
        let extra_requirements = self.get_extra_requirements()?;

        // Combine previous requirements with extra requirements
        let combined_requirements = HashSet::from_iter(self.build_requirements.iter().cloned())
            .union(&extra_requirements)
            .cloned()
            .collect::<Vec<_>>();

        // Install extra requirements if any new ones were found
        if !extra_requirements.is_empty()
            && self.build_requirements.len() != combined_requirements.len()
        {
            let locked_packages = HashMap::default();
            // Todo: use the previous resolve for the favored packages?
            let favored_packages = HashMap::default();
            let all_requirements = combined_requirements.to_vec();
            let extra_resolved_wheels = resolve(
                wheel_builder.package_db.clone(),
                all_requirements.iter(),
                wheel_builder.env_markers.clone(),
                wheel_builder.wheel_tags.clone(),
                locked_packages,
                favored_packages,
                wheel_builder.resolve_options.clone(),
                self.env_variables.clone(),
            )
            .await
            .map_err(|e| WheelBuildError::CouldNotResolveEnvironment(all_requirements, e))?;

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
                let (artifact, direct_url_json) = wheel_builder
                    .package_db
                    .get_wheel(artifact_info, Some(wheel_builder))
                    .await
                    .expect("could not get artifact");

                self.venv.install_wheel(
                    &artifact,
                    &UnpackWheelOptions {
                        direct_url_json,
                        ..Default::default()
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Run a command in the build environment
    pub(crate) fn run_command(&self, stage: &str) -> Result<Output, WheelBuildError> {
        // We modify the environment of the user
        // so that we can use the scripts directory to run the build frontend
        // e.g maturin depends on an executable in the scripts directory
        let script_path = self.venv.root().join(self.venv.install_paths().scripts());

        // PATH from env variables have higher priority over var_os one
        let env_path = if let Some(path) = self.env_variables.get("PATH") {
            Some(OsString::from(path))
        } else {
            std::env::var_os("PATH")
        };

        let path_var = match env_path {
            Some(path) => {
                let mut paths = std::env::split_paths(&path).collect::<Vec<_>>();
                paths.push(script_path);
                std::env::join_paths(paths.iter()).map_err(|e| {
                    WheelBuildError::CouldNotRunCommand(
                        stage.into(),
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("could not setup env path: {}", e),
                        ),
                    )
                })?
            }
            None => script_path.as_os_str().to_owned(),
        };

        let mut base_command = Command::new(self.venv.python_executable());
        if self.clean_env {
            base_command.env_clear();
        }
        let work_dir = self.work_dir.path();
        base_command
            .current_dir(&self.package_dir)
            // pass all env variables defined by user
            .envs(&self.env_variables)
            // even if PATH is present in self.env_variables
            // it will overwritten by more actual one
            .env("PATH", path_var)
            // Script to run
            .arg(work_dir.join("build_frontend.py"))
            // The working directory to use
            // will contain the output of the build
            .arg(work_dir.as_path())
            // Build system entry point
            .arg(&self.entry_point)
            // Building Wheel or Metadata
            .arg(stage)
            .output()
            .map_err(|e| WheelBuildError::CouldNotRunCommand(stage.into(), e))
    }

    fn default_build_system() -> pyproject_toml::BuildSystem {
        pyproject_toml::BuildSystem {
            requires: vec![
                Requirement {
                    name: "setuptools".into(),
                    extras: None,
                    marker: None,
                    version_or_url: None,
                },
                Requirement {
                    name: "wheel".into(),
                    extras: None,
                    marker: None,
                    version_or_url: None,
                },
            ],
            build_backend: Some("setuptools.build_meta:__legacy__".into()),
            backend_path: None,
        }
    }

    /// Setup the build environment so that we can build a wheel from an sdist
    pub(crate) async fn setup(
        sdist: &impl ArtifactFromSource,
        wheel_builder: &WheelBuilder,
    ) -> Result<BuildEnvironment, WheelBuildError> {
        // Setup a work directory and a new env dir
        let work_dir = tempfile::tempdir()?;
        let venv = VEnv::create(
            &work_dir.path().join("venv"),
            wheel_builder.resolve_options.python_location.clone(),
        )?;

        // Find the build system
        let build_system = sdist
            .read_pyproject_toml()
            .ok()
            .and_then(|t| t.build_system)
            .unwrap_or_else(Self::default_build_system);

        let build_system = if build_system.build_backend.is_none() {
            Self::default_build_system()
        } else {
            build_system
        };

        let entry_point = build_system
            .build_backend
            .clone()
            .expect("build_backend, cannot be None, this should never happen");

        // Find the build requirements
        let build_requirements = build_system.requires.clone();
        tracing::info!(
            "build requirements: {:?}",
            build_requirements
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
        );
        // Resolve the build environment
        let resolved_wheels = resolve(
            wheel_builder.package_db.clone(),
            build_requirements.iter(),
            wheel_builder.env_markers.clone(),
            wheel_builder.wheel_tags.clone(),
            HashMap::default(),
            HashMap::default(),
            wheel_builder.resolve_options.clone(),
            Default::default(),
        )
        .await
        .map_err(|e| {
            tracing::error!(
                "could not resolve build requirements when trying to build a wheel for : {}",
                sdist.artifact_name()
            );
            WheelBuildError::CouldNotResolveEnvironment(build_requirements.to_vec(), e)
        })?;

        // Install into venv
        for package_info in resolved_wheels.iter() {
            let artifact_info = package_info.artifacts.first().unwrap();

            let (artifact, _) = wheel_builder
                .package_db
                .get_wheel(artifact_info, Some(wheel_builder))
                .await
                .map_err(WheelBuildError::CouldNotGetArtifact)?;

            venv.install_wheel(
                &artifact,
                &UnpackWheelOptions {
                    installer: None,
                    ..Default::default()
                },
            )?;
        }

        // Package dir for the package we need to build
        let package_dir =
            work_dir
                .path()
                .join(format!("{}-{}", sdist.distribution_name(), sdist.version(),));

        let env_variables = if let Some(backend_path) = &build_system.backend_path {
            let mut env_variables = wheel_builder.env_variables.clone();
            // insert env var for the backend path that will be used by the build frontend
            env_variables.insert(
                "PEP517_BACKEND_PATH".into(),
                std::env::join_paths(normalize_backend_path(backend_path, &package_dir)?)?
                    .to_string_lossy()
                    .to_string(),
            );
            env_variables
        } else {
            wheel_builder.env_variables.clone()
        };

        Ok(BuildEnvironment {
            work_dir: TempBuildEnvironment::new(work_dir),
            package_dir,
            build_system,
            build_requirements,
            entry_point,
            resolved_wheels,
            venv,
            env_variables,
            clean_env: wheel_builder.resolve_options.clean_env,
            python_location: wheel_builder.resolve_options.python_location.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn test_norm_backend_path() {
        let package_dir = PathBuf::from("/home/user/project");
        let backend_path = vec![
            ".".to_string(),
            "./src".to_string(),
            "./backend".to_string(),
            "./build".to_string(),
        ];

        let normed = super::normalize_backend_path(&backend_path, &package_dir).unwrap();

        assert_eq!(
            normed,
            vec![
                PathBuf::from("/home/user/project"),
                PathBuf::from("/home/user/project/src"),
                PathBuf::from("/home/user/project/backend"),
                PathBuf::from("/home/user/project/build"),
            ]
        );

        let backend_path = vec!["../outside_pkg_dir".to_string()];
        super::normalize_backend_path(&backend_path, &package_dir).unwrap_err();

        let backend_path = vec!["/no_absolute_allowed".to_string()];
        super::normalize_backend_path(&backend_path, &package_dir).unwrap_err();
    }
}
