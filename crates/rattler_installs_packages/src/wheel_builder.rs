//! Turn an sdist into a wheel by creating a virtualenv and building the sdist in it
#![allow(missing_docs)]

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use crate::{
    sdist::SDist,
    venv::{PythonLocation, VEnv},
    Artifact, PackageDb, SDistName, Wheel, ArtifactInfo, core_metadata::WheelCoreMetadata,
};

type VEnvCache = RefCell<HashMap<SDistName, Rc<VEnv>>>;

/// A builder for wheels
pub struct WheelBuilder<'a> {
    /// A cache for virtualenvs that might be reused later in the process
    venv_cache: VEnvCache,

    /// The package database to use
    package_db: &'a PackageDb,
}

#[derive(thiserror::Error, Debug)]
pub enum WheelBuildError {
    #[error("Could not build wheel: {0}")]
    Error(String),
}

impl<'a> WheelBuilder<'a> {
    pub fn new(package_db: &'a PackageDb) -> Self {
        Self {
            venv_cache: RefCell::new(HashMap::new()),
            package_db,
        }
    }

    pub fn get_venv(&self, sdist: &SDist) -> Result<Rc<VEnv>, WheelBuildError> {
        if let Some(venv) = self.venv_cache.borrow().get(sdist.name()) {
            return Ok(venv.clone());
        } else {
            let venv_dir = tempfile::tempdir().unwrap();
            let venv = VEnv::create(venv_dir.path(), PythonLocation::System).unwrap();
            self.venv_cache
                .borrow_mut()
                .insert(sdist.name().clone(), Rc::new(venv));
            return Ok(self.venv_cache.borrow().get(sdist.name()).unwrap().clone());
        }
    }

    pub fn get_metadata(&self, sdist: &SDist) -> Result<(Vec<u8>, WheelCoreMetadata), WheelBuildError> {
        let venv = self.get_venv(sdist)?;
        println!("venv: {:?}", venv.python_executable());
        let build_info = sdist.read_build_info().expect("Could not read build info");
        println!("build_info: {:?}", build_info);

        // let mut pip = venv.pip();
        // pip.install(sdist)?;

        // let mut wheel = venv.wheel();
        // wheel.build(sdist)?;

        // wheel.get_metadata()
        Err(WheelBuildError::Error("Not implemented".to_string()))
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
