//! Turn an sdist into a wheel by creating a virtualenv and building the sdist in it
#![allow(missing_docs)]

use std::{
    cell::RefCell, collections::HashMap, path::PathBuf, process::Command, rc::Rc, str::FromStr,
};

use pep508_rs::{MarkerEnvironment, Requirement};

use crate::{
    core_metadata::WheelCoreMetadata,
    env_markers, resolve,
    sdist::SDist,
    tags::WheelTags,
    venv::{PythonLocation, VEnv},
    wheel::UnpackError,
    Artifact, ArtifactInfo, PackageDb, Pep508EnvMakers, ResolveOptions, SDistName, SDistResolution,
    UnpackWheelOptions, Wheel,
};

type VEnvCache = RefCell<HashMap<SDistName, Rc<VEnv>>>;

// include static build_frontend.py string
const BUILD_FRONTEND_PY: &str = include_str!("./wheel_builder_frontend.py");

/// A builder for wheels
pub struct WheelBuilder<'db, 'i> {
    /// A cache for virtualenvs that might be reused later in the process
    venv_cache: VEnvCache,

    /// The package database to use
    package_db: &'db PackageDb,

    env_markers: &'i MarkerEnvironment,

    wheel_tags: Option<&'i WheelTags>,

    resolve_options: ResolveOptions,
}

#[derive(thiserror::Error, Debug)]
pub enum WheelBuildError {
    #[error("Could not build wheel: {0}")]
    Error(String),

    #[error("Could not build wheel: {0}")]
    UnpackError(#[from] UnpackError),
}

enum WheelBuildGoal {
    GetRequiresForBuildWheel,
    WheelMetadata,
    Wheel,
}

impl<'db, 'i> WheelBuilder<'db, 'i> {
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
                ..resolve_options.clone()
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

    pub fn get_venv(&self, sdist: &SDist) -> Result<Rc<VEnv>, WheelBuildError> {
        let mut cache = self.venv_cache.borrow_mut();
        if let Some(venv) = cache.get(sdist.name()) {
            return Ok(venv.clone());
        } else {
            let venv_dir = tempfile::tempdir().unwrap();
            let venv = VEnv::create(venv_dir.path(), PythonLocation::System).unwrap();
            cache.insert(sdist.name().clone(), Rc::new(venv));
            return Ok(cache.get(sdist.name()).unwrap().clone());
        }
    }

    pub async fn get_metadata(
        &self,
        sdist: &SDist,
    ) -> Result<(Vec<u8>, WheelCoreMetadata), WheelBuildError> {
        let venv = self.get_venv(sdist)?;
        println!("venv: {:?}", venv.python_executable());
        let build_info = sdist.read_build_info().expect("Could not read build info");
        println!("build_info: {:?}", build_info);

        let requirements = if build_info.requires.is_empty() {
            vec![
                Requirement::from_str("setuptools").expect("Could not parse requirement"),
                Requirement::from_str("wheel").expect("Could not parse requirement"),
            ]
        } else {
            build_info.requires.clone()
        };

        println!("Resolving for {:?}", requirements);

        let resolved_wheels = resolve(
            self.package_db,
            requirements.iter(),
            &self.env_markers,
            self.wheel_tags,
            Default::default(),
            Default::default(),
            &self.resolve_options,
        )
        .await
        .expect("Could not resolve environment");

        println!("resolved_wheels: {:?}", resolved_wheels);

        // TODO: what's this?
        let options = UnpackWheelOptions { installer: None };

        for package_info in resolved_wheels {
            let artifact_info = package_info.artifacts.first().unwrap();
            let artifact = self
                .package_db
                .get_artifact::<Wheel>(artifact_info)
                .await
                .expect("Could not get artifact");
            venv.install_wheel(&artifact, &options)?;
        }

        let backend = build_info
            .build_backend
            .unwrap_or("setuptools.build_meta:__legacy__".to_string());

        let work_dir = tempfile::tempdir().expect("Could not set up tempdir");
        sdist.extract_to(work_dir.path()).expect("bla");

        std::fs::write(work_dir.path().join("build_frontend.py"), BUILD_FRONTEND_PY).expect("bla");

        let pkg_dir = work_dir.path().join(format!(
            "{}-{}",
            sdist.name().distribution.as_source_str(),
            sdist.name().version
        ));

        // three args: work_dir, goal
        let output = Command::new(venv.python_executable())
            .current_dir(&pkg_dir)
            .arg(work_dir.path().join("build_frontend.py"))
            .arg(work_dir.path())
            .arg(backend)
            .arg("WheelMetadata")
            .output()
            .expect("bla");

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(WheelBuildError::Error(format!(
                "Could not build wheel: {}",
                stdout
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let path = PathBuf::from(stdout.trim());
        let metadata = std::fs::read(&path).expect("bla");
        let wheel_metadata = WheelCoreMetadata::try_from(metadata.as_slice()).expect("bla");
        println!("wheel_metadata: {:?}", wheel_metadata);
        Ok((metadata, wheel_metadata))
        //     Ok(TemporaryVEnv {
        //         venv,
        //         work_dir: work_dir.into_path(),
        //         venv_dir,
        //     })

        // let mut pip = venv.pip();
        // pip.install(sdist)?;

        // let mut wheel = venv.wheel();
        // wheel.build(sdist)?;

        // wheel.get_metadata()
    }

    pub fn build_wheel(&self, sdist: &SDist) -> Result<(), WheelBuildError> {
        // let venv = VEnv::new(target);
        // venv.create()?;

        // let mut pip = venv.pip();
        // pip.install(sdist)?;

        // let mut wheel = venv.wheel();
        // wheel.build(sdist)?;

        // wheel.get_wheel_path()
        Ok(())
    }
}
