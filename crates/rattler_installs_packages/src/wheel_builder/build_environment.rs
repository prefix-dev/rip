use crate::artifacts::wheel::UnpackWheelOptions;
use crate::artifacts::SDist;

use crate::python_env::{PythonLocation, VEnv, WheelTags};
use crate::resolve::{resolve, PinnedPackage, ResolveOptions};
use crate::types::Artifact;
use crate::wheel_builder::{build_requirements, WheelBuildError, WheelBuilder};
use fs_err as fs;
use pep508_rs::{MarkerEnvironment, Requirement};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;

// include static build_frontend.py string
const BUILD_FRONTEND_PY: &str = include_str!("./wheel_builder_frontend.py");
/// A build environment for building wheels
/// This struct contains the virtualenv and everything that is needed
/// to execute the PEP517 build backend hools
#[derive(Debug)]
pub(crate) struct BuildEnvironment<'db> {
    work_dir: tempfile::TempDir,
    package_dir: PathBuf,
    #[allow(dead_code)]
    build_system: pyproject_toml::BuildSystem,
    entry_point: String,
    build_requirements: Vec<Requirement>,
    resolved_wheels: Vec<PinnedPackage<'db>>,
    venv: VEnv,
    env_variables: HashMap<String, String>,
    clean_env: bool,
    #[allow(dead_code)]
    python_location: PythonLocation,
}

impl<'db> BuildEnvironment<'db> {
    /// Extract the wheel and write the build_frontend.py to the work folder
    pub(crate) fn install_build_files(&self, sdist: &SDist) -> std::io::Result<()> {
        // Extract the sdist to the work folder
        sdist.extract_to(self.work_dir.path())?;
        // Write the python frontend to the work folder
        fs::write(
            self.work_dir.path().join("build_frontend.py"),
            BUILD_FRONTEND_PY,
        )
    }

    pub(crate) fn work_dir(&self) -> &Path {
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

    /// Install extra requirements into the venv, if any extra were found
    /// If the extra requirements are already installed, this will do nothing
    /// for that requirement.
    pub(crate) async fn install_extra_requirements<'i>(
        &self,
        wheel_builder: &WheelBuilder<'db, 'i>,
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

        // Install extra requirements if any new ones were found
        if !extra_requirements.is_empty()
            && self.build_requirements.len() != combined_requirements.len()
        {
            let locked_packages = HashMap::default();
            // Todo: use the previous resolve for the favored packages?
            let favored_packages = HashMap::default();
            let all_requirements = combined_requirements.to_vec();
            let extra_resolved_wheels = resolve(
                wheel_builder.package_db,
                all_requirements.iter(),
                env_markers,
                wheel_tags,
                locked_packages,
                favored_packages,
                resolve_options,
                self.env_variables.clone(),
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
                let artifact = wheel_builder
                    .package_db
                    .get_wheel(artifact_info, Some(wheel_builder))
                    .await
                    .expect("could not get artifact");

                self.venv
                    .install_wheel(&artifact, &UnpackWheelOptions::default())?;
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
        base_command
            .current_dir(&self.package_dir)
            // pass all env variables defined by user
            .envs(&self.env_variables)
            // even if PATH is present in self.env_variables
            // it will overwritten by more actual one
            .env("PATH", path_var)
            // Script to run
            .arg(self.work_dir.path().join("build_frontend.py"))
            // The working directory to use
            // will contain the output of the build
            .arg(self.work_dir.path())
            // Build system entry point
            .arg(&self.entry_point)
            // Building Wheel or Metadata
            .arg(stage)
            .output()
            .map_err(|e| WheelBuildError::CouldNotRunCommand(stage.into(), e))
    }

    /// Setup the build environment so that we can build a wheel from an sdist
    pub(crate) async fn setup<'i>(
        sdist: &SDist,
        wheel_builder: &WheelBuilder<'db, 'i>,
        env_markers: &MarkerEnvironment,
        wheel_tags: Option<&WheelTags>,
        resolve_options: &ResolveOptions,
        env_variables: HashMap<String, String>,
    ) -> Result<BuildEnvironment<'db>, WheelBuildError> {
        // Setup a work directory and a new env dir
        let work_dir = tempfile::tempdir().unwrap();
        let venv = VEnv::create(
            &work_dir.path().join("venv"),
            resolve_options.python_location.clone(),
        )?;

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
        tracing::info!(
            "build requirements: {:?}",
            build_requirements
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
        );
        // Resolve the build environment
        let resolved_wheels = resolve(
            wheel_builder.package_db,
            build_requirements.iter(),
            env_markers,
            wheel_tags,
            HashMap::default(),
            HashMap::default(),
            resolve_options,
            Default::default(),
        )
        .await
        .map_err(|_| WheelBuildError::CouldNotResolveEnvironment(build_requirements.to_vec()))?;

        // Install into venv
        for package_info in resolved_wheels.iter() {
            let artifact_info = package_info.artifacts.first().unwrap();

            let artifact = wheel_builder
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
            env_variables,
            clean_env: resolve_options.clean_env,
            python_location: resolve_options.python_location.clone(),
        })
    }
}
