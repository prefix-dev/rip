use crate::python_env::PythonInterpreterVersion;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// A struct of installation categories to where they should be stored relative to the
/// installation destination.
#[derive(Debug, Clone)]
pub struct InstallPaths {
    purelib: PathBuf,
    platlib: PathBuf,
    scripts: PathBuf,
    data: PathBuf,
    headers: PathBuf,
    windows: bool,
}

impl InstallPaths {
    /// Populates mappings of installation targets for a virtualenv layout. The mapping depends on
    /// the python version and whether or not the installation targets windows. Specifically on
    /// windows some of the paths are different. :shrug:
    pub fn for_venv<V: Into<PythonInterpreterVersion>>(version: V, windows: bool) -> Self {
        let version = version.into();

        let site_packages = if windows {
            Path::new("Lib").join("site-packages")
        } else {
            Path::new("lib").join(format!(
                "python{}.{}/site-packages",
                version.major, version.minor
            ))
        };
        let scripts = if windows {
            PathBuf::from("Scripts")
        } else {
            PathBuf::from("bin")
        };

        // Data should just be the root of the venv
        let data = PathBuf::from("");

        // purelib and platlib locations are not relevant when using venvs
        // https://stackoverflow.com/a/27882460/3549270
        Self {
            purelib: site_packages.clone(),
            platlib: site_packages,
            scripts,
            data,
            windows,
            headers: PathBuf::from("include"),
        }
    }

    /// Determines whether this is a windows InstallPath
    pub fn is_windows(&self) -> bool {
        self.windows
    }

    /// Returns the site-packages location. This is done by searching for the purelib location.
    pub fn site_packages(&self) -> &Path {
        &self.purelib
    }

    /// Reference to pure python library location.
    pub fn purelib(&self) -> &Path {
        &self.purelib
    }

    /// Reference to platform specific library location.
    pub fn platlib(&self) -> &Path {
        &self.platlib
    }

    /// Returns the binaries location.
    pub fn scripts(&self) -> &Path {
        &self.scripts
    }

    /// Returns the location of the data directory
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// Returns the location of the include directory
    pub fn include(&self) -> PathBuf {
        if self.windows {
            PathBuf::from("Include")
        } else {
            PathBuf::from("include")
        }
    }

    /// Returns the location of the headers directory. The location of headers is specific to a
    /// distribution name.
    pub fn headers(&self, distribution_name: &str) -> PathBuf {
        self.headers.join(distribution_name)
    }

    /// Matches the different categories to their install paths.
    pub fn match_category(&self, category: &str, distribution_name: &str) -> Option<Cow<Path>> {
        match category {
            "purelib" => Some(self.purelib().into()),
            "platlib" => Some(self.platlib().into()),
            "scripts" => Some(self.scripts().into()),
            "data" => Some(self.data().into()),
            "headers" => Some(self.headers(distribution_name).into()),
            &_ => None,
        }
    }
}
