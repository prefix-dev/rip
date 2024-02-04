//! Module for working with python environments.
//! Contains functionality for querying and manipulating python environments.

mod tags;

mod distribution_finder;

mod env_markers;

mod system_python;

mod uninstall;
mod venv;

mod byte_code_compiler;

pub use tags::{WheelTag, WheelTags};

pub use byte_code_compiler::{ByteCodeCompiler, CompilationError, SpawnCompilerError};
pub use distribution_finder::{
    find_distributions_in_directory, find_distributions_in_venv, Distribution,
    FindDistributionError,
};
pub use env_markers::Pep508EnvMakers;
pub(crate) use system_python::{system_python_executable, FindPythonError};
pub use system_python::{ParsePythonInterpreterVersionError, PythonInterpreterVersion};
pub use uninstall::{uninstall_distribution, UninstallDistributionError};
pub use venv::{PythonLocation, VEnv, VEnvError};
