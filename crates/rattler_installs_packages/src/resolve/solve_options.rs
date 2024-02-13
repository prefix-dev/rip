//! Contains the options that can be passed to the [`super::solve::resolve`] function.

use crate::{python_env::PythonLocation, types::NormalizedPackageName};
use pep508_rs::{Requirement, VersionOrUrl};
use std::{collections::HashMap, str::FromStr};

use crate::types::PackageName;

use super::PinnedPackage;

/// Defines how to handle sdists during resolution.
#[derive(Default, Debug, Clone, Copy, Eq, PartialOrd, PartialEq)]
pub enum SDistResolution {
    /// Both versions with wheels and/or sdists are allowed to be selected during resolution. But
    /// during resolution the metadata from wheels is preferred over sdists.
    ///
    /// If we have the following scenario:
    ///
    /// ```txt
    /// Version@1
    /// - WheelA
    /// - WheelB
    /// Version@2
    /// - SDist
    /// - WheelA
    /// - WheelB
    /// Version@3
    /// - SDist
    /// ```
    ///
    /// Then the Version@3 will be selected because it has the highest version. This option makes no
    /// distinction between whether the version has wheels or sdist.
    #[default]
    Normal,

    /// Allow sdists to be selected during resolution but only if all versions with wheels cannot
    /// be selected. This means that even if a higher version is technically available it might not
    /// be selected if it only has an available sdist.
    ///
    /// If we have the following scenario:
    ///
    /// ```txt
    /// Version@1
    /// - SDist
    /// - WheelA
    /// - WheelB
    /// Version@2
    /// - SDist
    /// ```
    ///
    /// Then the Version@1 will be selected even though the highest version is 2. This is because
    /// version 2 has no available wheels. If version 1 would not exist though then version 2 is
    /// selected because there are no other versions with a wheel.
    PreferWheels,

    /// Allow sdists to be selected during resolution and prefer them over wheels. This means that
    /// even if a higher version is available but it only includes wheels it might not be selected.
    ///
    /// If we have the following scenario:
    ///
    /// ```txt
    /// Version@1
    /// - SDist
    /// - WheelA
    /// Version@2
    /// - WheelA
    /// ```
    ///
    /// Then the version@1 will be selected even though the highest version is 2. This is because
    /// version 2 has no sdists available. If version 1 would not exist though then version 2 is
    /// selected because there are no other versions with an sdist.
    PreferSDists,

    /// Don't select sdists during resolution
    ///
    /// If we have the following scenario:
    ///
    /// ```txt
    /// Version@1
    /// - SDist
    /// - WheelA
    /// - WheelB
    /// Version@2
    /// - SDist
    /// ```
    ///
    /// Then version 1 will be selected because it has wheels and version 2 does not. If version 1
    /// would not exist there would be no solution because none of the versions have wheels.
    OnlyWheels,

    /// Only select sdists during resolution
    ///
    /// If we have the following scenario:
    ///
    /// ```txt
    /// Version@1
    /// - SDist
    /// Version@2
    /// - WheelA
    /// ```
    ///
    /// Then version 1 will be selected because it has an sdist and version 2 does not. If version 1
    /// would not exist there would be no solution because none of the versions have sdists.
    OnlySDists,
}

/// Defines how to pre-releases are handled during package resolution.
#[derive(Debug, Clone, Eq, PartialOrd, PartialEq)]
pub enum PreReleaseResolution {
    /// Don't allow pre-releases to be selected during resolution
    Disallow,

    /// Conditionally allow pre-releases to be selected during resolution. This
    /// behavior emulates `pip`'s pre-release resolution, which is not according
    /// to "spec" but the most widely used logic.
    ///
    /// It works as follows:
    ///
    /// - if a version specifier mentions a pre-release, then we allow
    ///   pre-releases to be selected, for example `jupyterlab==4.1.0b0` will
    ///   allow the selection of the `jupyterlab-4.1.0b0` beta release during
    ///   resolution.
    /// - if a package _only_ contains pre-release versions then we allow
    ///   pre-releases to be selected for any version specifier. For example, if
    ///   the package `supernew` only contains `supernew-1.0.0b0` and
    ///   `supernew-1.0.0b1` then we allow `supernew==1.0.0` to select
    ///   `supernew-1.0.0b1` during resolution.
    /// - Any name that is mentioned in the `allow` list will allow pre-releases (this
    ///   is usually derived from the specs given by the user). For example, if the user
    ///   asks for `foo>0.0.0b0`, pre-releases are globally enabled for package foo (also as
    ///   transitive dependency).
    AllowIfNoOtherVersionsOrEnabled {
        /// A list of package names that will allow pre-releases to be selected
        allow_names: Vec<String>,
    },

    /// Allow any pre-releases to be selected during resolution
    Allow,
}

impl Default for PreReleaseResolution {
    fn default() -> Self {
        PreReleaseResolution::AllowIfNoOtherVersionsOrEnabled {
            allow_names: Vec::new(),
        }
    }
}

impl PreReleaseResolution {
    /// Return a AllowIfNoOtherVersionsOrEnabled variant from a list of requirements
    pub fn from_specs(specs: &[Requirement]) -> Self {
        let mut allow_names = Vec::new();
        for spec in specs {
            match &spec.version_or_url {
                Some(VersionOrUrl::VersionSpecifier(v)) => {
                    if v.iter().any(|s| s.version().any_prerelease()) {
                        let name = PackageName::from_str(&spec.name).expect("invalid package name");
                        allow_names.push(name.as_str().to_string());
                    }
                }
                _ => continue,
            };
        }
        PreReleaseResolution::AllowIfNoOtherVersionsOrEnabled { allow_names }
    }
}

impl SDistResolution {
    /// Returns true if sdists are allowed to be selected during resolution
    pub fn allow_sdists(&self) -> bool {
        !matches!(self, SDistResolution::OnlyWheels)
    }

    /// Returns true if sdists are allowed to be selected during resolution
    pub fn allow_wheels(&self) -> bool {
        !matches!(self, SDistResolution::OnlySDists)
    }
}

/// Specifies what to do with failed build environments
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OnWheelBuildFailure {
    /// Save failed build environments to temporary directory
    SaveBuildEnv,
    /// Delete failed build environments
    #[default]
    DeleteBuildEnv,
}

/// Additional options that may influence the solver. In general passing [`Default::default`] to
/// the [`super::resolve`] function should provide sane defaults, however if you want to fine tune the
/// resolver you can do so via this struct.
#[derive(Default, Clone)]
pub struct ResolveOptions {
    /// Defines how to handle sdists during resolution. By default sdists will be treated the same
    /// as wheels.
    pub sdist_resolution: SDistResolution,

    /// Defines what python interpreter to use for resolution. By default the python interpreter
    /// from the system is used. This is only used during resolution and building of wheel files
    pub python_location: PythonLocation,

    /// Defines if we should inherit env variables during build process of wheel files
    pub clean_env: bool,

    /// Defines what to do with failed build environments
    /// by default these are deleted but can also be saved for debugging purposes
    pub on_wheel_build_failure: OnWheelBuildFailure,

    /// Defines whether pre-releases are allowed to be selected during resolution. By default
    /// pre-releases are not allowed (only if there are no other versions available for a given dependency).
    pub pre_release_resolution: PreReleaseResolution,

    /// Defines locked packages that should be used
    pub locked_packages: HashMap<NormalizedPackageName, PinnedPackage>,

    /// Defines favored packages that should be used
    pub favored_packages: HashMap<NormalizedPackageName, PinnedPackage>,

    /// Defines env variables that can be used during resolving
    pub env_variables: HashMap<String, String>,
}

impl ResolveOptions {
    /// Change resolve options locked packages
    pub fn with_locked_packages(
        &mut self,
        locked_packages: HashMap<NormalizedPackageName, PinnedPackage>,
    ) -> &mut Self {
        self.locked_packages = locked_packages;
        self
    }

    /// Change resolve options favored packages
    pub fn with_favored_packages(
        &mut self,
        favored_packages: HashMap<NormalizedPackageName, PinnedPackage>,
    ) -> &mut Self {
        self.favored_packages = favored_packages;
        self
    }

    /// Change env variables of resolve options
    pub fn with_env_variables(&mut self, env_variables: HashMap<String, String>) -> &mut Self {
        self.env_variables = env_variables;
        self
    }
}
