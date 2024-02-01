use super::dependency_provider::{PypiPackageName, PypiVersionSet};
use crate::index::PackageDb;
use crate::python_env::{PythonLocation, WheelTags};
use crate::resolve::dependency_provider::PypiDependencyProvider;
use crate::resolve::PypiVersion;
use crate::types::PackageName;
use crate::{types::ArtifactInfo, types::Extra, types::NormalizedPackageName};
use elsa::FrozenMap;
use pep508_rs::{MarkerEnvironment, Requirement, VersionOrUrl};
use resolvo::{DefaultSolvableDisplay, Pool, Solver, UnsolvableOrCancelled};
use std::collections::HashMap;
use std::str::FromStr;

use std::collections::HashSet;
use std::ops::Deref;
use std::sync::Arc;

/// Represents a single locked down distribution (python package) after calling [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedPackage {
    /// The name of the package
    pub name: NormalizedPackageName,

    /// The selected version
    pub version: PypiVersion,

    /// The possible direct URL for it
    // pub url: Option<Url>,

    /// The extras that where selected either by the user or as part of the resolution.
    pub extras: HashSet<Extra>,

    /// The applicable artifacts for this package. These have been ordered by compatibility if
    /// `compatible_tags` have been provided to the solver.
    ///
    /// This list may be empty if the package was locked or favored.
    pub artifacts: Vec<Arc<ArtifactInfo>>,
}

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
/// the [`resolve`] function should provide sane defaults, however if you want to fine tune the
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
}

/// Resolves an environment that contains the given requirements and all dependencies of those
/// requirements.
///
/// `requirements` defines the requirements of packages that must be present in the solved
/// environment.
/// `env_markers` defines information about the python interpreter.
///
/// If `compatible_tags` is defined then the available artifacts of a distribution are filtered to
/// include only artifacts that are compatible with the specified tags. If `None` is passed, the
/// artifacts are not filtered at all
// TODO: refactor this into an input type of sorts later
#[allow(clippy::too_many_arguments)]
pub async fn resolve(
    package_db: Arc<PackageDb>,
    requirements: impl IntoIterator<Item = &Requirement>,
    env_markers: Arc<MarkerEnvironment>,
    compatible_tags: Option<Arc<WheelTags>>,
    locked_packages: HashMap<NormalizedPackageName, PinnedPackage>,
    favored_packages: HashMap<NormalizedPackageName, PinnedPackage>,
    options: ResolveOptions,
    env_variables: HashMap<String, String>,
) -> miette::Result<Vec<PinnedPackage>> {
    // Construct the pool
    let pool = Pool::new();

    // Construct HashMap of Name to URL
    let name_to_url: FrozenMap<NormalizedPackageName, String> = FrozenMap::default();

    // Construct the root requirements from the requirements requested by the user.
    let requirements = requirements.into_iter();
    let requirement_count = requirements.size_hint();
    let mut root_requirements =
        Vec::with_capacity(requirement_count.1.unwrap_or(requirement_count.0));

    for Requirement {
        name,
        version_or_url,
        extras,
        ..
    } in requirements
    {
        let name = PackageName::from_str(name).expect("invalid package name");
        let pypi_name = PypiPackageName::Base(name.clone().into());
        let dependency_package_name = pool.intern_package_name(pypi_name.clone());
        let version_set_id = pool.intern_version_set(
            dependency_package_name,
            PypiVersionSet::from_spec(version_or_url.clone(), &options.pre_release_resolution),
        );
        root_requirements.push(version_set_id);

        if let Some(VersionOrUrl::Url(url)) = version_or_url {
            name_to_url.insert(pypi_name.base().clone(), url.clone().as_str().to_owned());
        }

        for extra in extras.iter().flatten() {
            let extra: Extra = extra.parse().expect("invalid extra");
            let dependency_package_name = pool
                .intern_package_name(PypiPackageName::Extra(name.clone().into(), extra.clone()));
            let version_set_id = pool.intern_version_set(
                dependency_package_name,
                PypiVersionSet::from_spec(version_or_url.clone(), &options.pre_release_resolution),
            );
            root_requirements.push(version_set_id);
        }
    }

    // Construct the provider
    let provider = PypiDependencyProvider::new(
        pool,
        package_db,
        env_markers,
        compatible_tags,
        locked_packages,
        favored_packages,
        name_to_url,
        options,
        env_variables,
    )?;

    // Invoke the solver to get a solution to the requirements
    let mut solver = Solver::new(&provider);
    let solvables = match solver.solve(root_requirements) {
        Ok(solvables) => solvables,
        Err(e) => {
            return match e {
                UnsolvableOrCancelled::Unsolvable(problem) => Err(miette::miette!(
                    "{}",
                    problem
                        .display_user_friendly(&solver, &DefaultSolvableDisplay)
                        .to_string()
                        .trim()
                )),
                UnsolvableOrCancelled::Cancelled(e) => {
                    let e = e.downcast::<crate::resolve::dependency_provider::MetadataError>().expect("invalid cancellation error message, expected a MetadataError, this indicates an error in the code");
                    let report = e.deref().clone().into();
                    Err(report)
                }
            };
        }
    };
    let mut result: HashMap<NormalizedPackageName, PinnedPackage> = HashMap::new();
    for solvable_id in solvables {
        let pool = solver.pool();
        let solvable = pool.resolve_solvable(solvable_id);
        let name = pool.resolve_package_name(solvable.name_id());
        let version = solvable.inner();

        // Get the entry in the result
        let entry = result
            .entry(name.base().clone())
            .or_insert_with(|| PinnedPackage {
                name: name.base().clone(),
                version: version.clone(),
                extras: Default::default(),
                artifacts: provider
                    .cached_artifacts
                    .get(&solvable_id)
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect(),
            });

        // Add the extra if selected
        if let PypiPackageName::Extra(_, extra) = name {
            entry.extras.insert(extra.clone());
        }
    }

    Ok(result.into_values().collect())
}

#[cfg(test)]
mod test {}
