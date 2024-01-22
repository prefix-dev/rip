//! Module that helps with allowing in the creation of python virtual environments.
//! Now just use the python venv command to create the virtual environment.
//! Later on we can look into actually creating the environment by linking to the python library,
//! and creating the necessary files. See: [VEnv](https://packaging.python.org/en/latest/specifications/virtual-environments/#declaring-installation-environments-as-python-virtual-environments)
use crate::artifacts::wheel::{InstallPaths, UnpackWheelOptions, Wheel};
use crate::artifacts::wheel::{UnpackError, UnpackedWheel};
use crate::python_env::{
    system_python_executable, FindPythonError, ParsePythonInterpreterVersionError,
    PythonInterpreterVersion,
};
use fs_err as fs;
use std::ffi::OsStr;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use thiserror::Error;

#[cfg(unix)]
pub fn copy_file<P: AsRef<Path>, U: AsRef<Path>>(from: P, to: U) -> std::io::Result<()> {
    fs::os::unix::fs::symlink(from, to)?;
    Ok(())
}

#[cfg(windows)]
pub fn copy_file<P: AsRef<Path>, U: AsRef<Path>>(from: P, to: U) -> std::io::Result<()> {
    fs::copy(from, to)?;
    Ok(())
}

/// Specifies where to find the python executable
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PythonLocation {
    /// Use system interpreter
    #[default]
    System,
    /// Use custom interpreter
    Custom(PathBuf),

    /// Use custom interpreter with version
    CustomWithVersion(PathBuf, PythonInterpreterVersion),
}

impl PythonLocation {
    /// Location of python executable
    pub fn executable(&self) -> Result<PathBuf, FindPythonError> {
        match self {
            PythonLocation::System => system_python_executable().cloned(),
            PythonLocation::Custom(path) => Ok(path.clone()),
            PythonLocation::CustomWithVersion(path, _) => Ok(path.clone()),
        }
    }

    /// Get python version from executable
    pub fn version(&self) -> Result<PythonInterpreterVersion, ParsePythonInterpreterVersionError> {
        match self {
            PythonLocation::System => PythonInterpreterVersion::from_system(),
            PythonLocation::CustomWithVersion(_, version) => Ok(version.clone()),
            PythonLocation::Custom(path) => PythonInterpreterVersion::from_path(path),
        }
    }
}

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum VEnvError {
    #[error(transparent)]
    FindPythonError(#[from] FindPythonError),
    #[error(transparent)]
    ParsePythonInterpreterVersionError(#[from] ParsePythonInterpreterVersionError),
    #[error(transparent)]
    FailedToCreate(#[from] std::io::Error),
}

/// Represents a virtual environment in which wheels can be installed
#[derive(Debug)]
pub struct VEnv {
    /// Location of the virtual environment
    location: PathBuf,
    /// Install paths for this virtual environment
    install_paths: InstallPaths,
}

impl VEnv {
    fn new(location: PathBuf, install_paths: InstallPaths) -> Self {
        Self {
            location,
            install_paths,
        }
    }

    /// Install a wheel into this virtual environment
    pub fn install_wheel(
        &self,
        wheel: &Wheel,
        options: &UnpackWheelOptions,
    ) -> Result<UnpackedWheel, UnpackError> {
        wheel.unpack(
            &self.location,
            &self.install_paths,
            &self.python_executable(),
            options,
        )
    }

    /// Execute python script in venv
    pub fn execute_script(&self, script: &Path) -> std::io::Result<Output> {
        let mut cmd = Command::new(self.python_executable());
        cmd.arg(script);
        cmd.output()
    }

    /// Execute python command in venv
    pub fn execute_command<S: AsRef<str>>(&self, command: S) -> std::io::Result<Output> {
        let mut cmd = Command::new(self.python_executable());
        cmd.arg("-c");
        cmd.arg(command.as_ref());
        cmd.output()
    }

    /// Returns the [`InstallPaths`] that defines some of the common paths in the virtual env.
    pub fn install_paths(&self) -> &InstallPaths {
        &self.install_paths
    }

    /// Returns the root directory of the virtual env.
    pub fn root(&self) -> &Path {
        &self.location
    }

    /// Path to python executable in venv
    pub fn python_executable(&self) -> PathBuf {
        let executable = if self.install_paths.is_windows() {
            "python.exe"
        } else {
            "python"
        };
        self.location
            .join(self.install_paths.scripts())
            .join(executable)
    }

    /// Create a virtual environment at specified directory
    /// for the platform we are running on
    pub fn create(venv_dir: &Path, python: PythonLocation) -> Result<VEnv, VEnvError> {
        Self::create_custom(venv_dir, python, cfg!(windows))
    }

    /// Create a virtual environment at specified directory
    /// allows specifying if this is a windows venv
    pub fn create_custom(
        venv_abs_dir: &Path,
        python: PythonLocation,
        windows: bool,
    ) -> Result<VEnv, VEnvError> {
        let base_python_path = python.executable()?;
        let base_python_version = PythonInterpreterVersion::from_path(&base_python_path)?;
        let base_python_name = base_python_path
            .file_name()
            .expect("Cannot extract base python name");

        let install_paths = InstallPaths::for_venv(base_python_version.clone(), windows);

        Self::create_install_paths(venv_abs_dir, &install_paths)?;
        Self::create_pyvenv(venv_abs_dir, &base_python_path, base_python_version.clone())?;

        let exe_path = install_paths.scripts().join(base_python_name);
        let abs_exe_path = venv_abs_dir.join(exe_path);

        {
            Self::setup_python(&abs_exe_path, &base_python_path, base_python_version)?;
        }

        Ok(VEnv::new(venv_abs_dir.to_path_buf(), install_paths))
    }

    /// Create all directories based on venv install paths mapping
    pub fn create_install_paths(
        venv_abs_path: &Path,
        install_paths: &InstallPaths,
    ) -> std::io::Result<()> {
        if !venv_abs_path.exists() {
            fs::create_dir_all(venv_abs_path)?;
        }

        let libpath = Path::new(&venv_abs_path).join(install_paths.site_packages());
        let include_path = Path::new(&venv_abs_path).join(install_paths.include());
        let bin_path = Path::new(&venv_abs_path).join(install_paths.scripts());

        let paths_to_create = [libpath, include_path, bin_path];

        for path in paths_to_create.iter() {
            if !path.exists() {
                fs::create_dir_all(path)?;
            }
        }

        // https://bugs.python.org/issue21197
        // create lib64 as a symlink to lib on 64-bit non-OS X POSIX
        #[cfg(all(target_pointer_width = "64", unix, not(target_os = "macos")))]
        {
            let lib64 = venv_abs_path.join("lib64");
            if !lib64.exists() {
                std::os::unix::fs::symlink("lib", lib64)?;
            }
        }

        Ok(())
    }

    /// Create pyvenv.cfg and write it's content based on system python
    pub fn create_pyvenv(
        venv_path: &Path,
        python_path: &Path,
        python_version: PythonInterpreterVersion,
    ) -> std::io::Result<()> {
        let venv_name = venv_path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| {
                std::io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "cannot extract base name from venv path {}",
                        venv_path.display()
                    ),
                )
            })?;

        let pyenv_cfg_content = format!(
            r#"
home = {}
include-system-site-packages = false
version = {}.{}.{}
prompt = {}"#,
            python_path
                .parent()
                .expect("system python path should have parent folder")
                .display(),
            python_version.major,
            python_version.minor,
            python_version.patch,
            venv_name,
        );

        let cfg_path = Path::new(&venv_path).join("pyvenv.cfg");
        fs_err::write(cfg_path, pyenv_cfg_content)?;
        Ok(())
    }

    /// Copy original python executable and populate other suffixed binaries
    pub fn setup_python(
        venv_exe_path: &Path,
        original_python_exe: &Path,
        python_version: PythonInterpreterVersion,
    ) -> std::io::Result<()> {
        let venv_bin = venv_exe_path
            .parent()
            .expect("venv exe binary should have parent folder");

        #[cfg(not(windows))]
        {
            if !venv_exe_path.exists() {
                copy_file(original_python_exe, venv_exe_path)?;
            }

            let python_bins = [
                "python",
                "python3",
                &format!("python{}.{}", python_version.major, python_version.minor).to_string(),
            ];

            for bin_name in python_bins.into_iter() {
                let venv_python_bin = venv_bin.join(bin_name);
                if !venv_python_bin.exists() && venv_exe_path != venv_python_bin {
                    copy_file(venv_exe_path, &venv_python_bin)?;
                }
            }
        }

        #[cfg(windows)]
        {
            if python_version.major <= 3 && python_version.minor <= 7 && python_version.patch <= 4 {
                tracing::warn!(
                    "Creation of venv for <=3.7.4 on windows may fail. Please use newer version"
                );
            }
            let base_exe_name = venv_exe_path
                .file_name()
                .expect("cannot get windows venv exe name");
            let python_bins = [
                "python.exe",
                "python_d.exe",
                "pythonw.exe",
                "pythonw_d.exe",
                base_exe_name
                    .to_str()
                    .expect("cannot convert windows venv exe name"),
            ];

            let original_python_bin_dir = original_python_exe
                .parent()
                .expect("cannot get system python parent folder");
            for bin_name in python_bins.into_iter() {
                let original_python_bin = original_python_bin_dir.join(bin_name);
                let original_python_scripts = original_python_bin_dir
                    .join("Lib/venv/scripts/nt")
                    .join(bin_name);
                let venv_python_bin = venv_bin.join(bin_name);

                if original_python_bin.exists() && !venv_python_bin.exists() {
                    if original_python_scripts.is_file() {
                        copy_file(original_python_scripts, &venv_python_bin)?;
                    } else {
                        // If used python is built from source code
                        // or we are using a python from venv which
                        // was created using python from source code
                        // we need to move venvlauncher
                        // and venvwlauncher as python.exe and pythonw.exe
                        // instead of using the python.exe shipped with /venv/scripts/nt
                        let launcher_bin_name = if bin_name.contains("python") {
                            bin_name.replace("python", "venvlauncher")
                        } else if bin_name.contains("pythonw") {
                            bin_name.replace("pythonw", "venvwlauncher")
                        } else {
                            bin_name.to_owned()
                        };
                        let original_launcher_bin = original_python_bin_dir.join(launcher_bin_name);
                        if original_launcher_bin.exists() {
                            copy_file(original_launcher_bin, &venv_python_bin)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::VEnv;
    use crate::python_env::PythonLocation;
    use crate::types::NormalizedPackageName;
    use std::env;
    use std::path::Path;
    use std::str::FromStr;

    #[test]
    pub fn venv_creation() {
        let venv_dir = tempfile::tempdir().unwrap();
        let venv = VEnv::create(venv_dir.path(), PythonLocation::System).unwrap();

        // Does python exist
        assert!(venv.python_executable().is_file());

        // Install wheel
        let wheel = crate::artifacts::Wheel::from_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../test-data/wheels/wordle_python-2.3.32-py3-none-any.whl"),
            &NormalizedPackageName::from_str("wordle_python").unwrap(),
        )
        .unwrap();
        venv.install_wheel(&wheel, &Default::default()).unwrap();

        // See if it worked
        let output = venv
            .execute_script(
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../test-data/scripts/test_wordle.py"),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "('A   d   i   E   u   ', False)"
        );
    }

    #[test]
    pub fn test_python_set_env_prefix() {
        let venv_dir = tempfile::tempdir().unwrap();

        let venv = VEnv::create(venv_dir.path(), PythonLocation::System).unwrap();

        let base_prefix_output = venv
            .execute_command("import sys; print(sys.base_prefix, end='')")
            .unwrap();
        let base_prefix = String::from_utf8_lossy(&base_prefix_output.stdout);

        let venv_prefix_output = venv
            .execute_command("import sys; print(sys.prefix, end='')")
            .unwrap();
        let venv_prefix = String::from_utf8_lossy(&venv_prefix_output.stdout);

        assert!(
            base_prefix != venv_prefix,
            "base prefix of venv should be different from prefix"
        )
    }

    #[test]
    pub fn test_python_install_paths_are_created() {
        let venv_dir = tempfile::tempdir().unwrap();

        let venv = VEnv::create(venv_dir.path(), PythonLocation::System).unwrap();
        let install_paths = venv.install_paths;

        let platlib_path = venv_dir.path().join(install_paths.platlib());
        let scripts_path = venv_dir.path().join(install_paths.scripts());
        let include_path = venv_dir.path().join(install_paths.include());

        assert!(platlib_path.exists(), "platlib path is not created");
        assert!(scripts_path.exists(), "scripts path is not created");
        assert!(include_path.exists(), "include path is not created");
    }

    #[test]
    pub fn test_same_venv_can_be_created_twice() {
        let venv_dir = tempfile::tempdir().unwrap();

        let venv = VEnv::create(venv_dir.path(), PythonLocation::System).unwrap();
        let another_same_venv = VEnv::create(venv_dir.path(), PythonLocation::System).unwrap();

        assert!(
            venv.location == another_same_venv.location,
            "same venv was not created in same location"
        )
    }
}
