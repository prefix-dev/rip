use super::dependency_provider::PypiPackageName;
use crate::index::PackageDb;
use crate::python_env::{PythonLocation, WheelTags};
use crate::resolve::dependency_provider::{PypiDependencyProvider, PypiVersion};
use crate::types::PackageName;
use crate::{types::ArtifactInfo, types::Extra, types::NormalizedPackageName, types::Version};
use pep508_rs::{MarkerEnvironment, Requirement};
use resolvo::{DefaultSolvableDisplay, Solver, UnsolvableOrCancelled};
use std::collections::HashMap;
use std::str::FromStr;

use std::collections::HashSet;

/// Represents a single locked down distribution (python package) after calling [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedPackage<'db> {
    /// The name of the package
    pub name: NormalizedPackageName,

    /// The selected version
    pub version: Version,

    /// The extras that where selected either by the user or as part of the resolution.
    pub extras: HashSet<Extra>,

    /// The applicable artifacts for this package. These have been ordered by compatibility if
    /// `compatible_tags` have been provided to the solver.
    ///
    /// This list may be empty if the package was locked or favored.
    pub artifacts: Vec<&'db ArtifactInfo>,
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
pub async fn resolve<'db>(
    package_db: &'db PackageDb,
    requirements: impl IntoIterator<Item = &Requirement>,
    env_markers: &MarkerEnvironment,
    compatible_tags: Option<&WheelTags>,
    locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
    favored_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
    options: &ResolveOptions,
    env_variables: HashMap<String, String>,
) -> miette::Result<Vec<PinnedPackage<'db>>> {
    // Construct a provider
    let provider = PypiDependencyProvider::new(
        package_db,
        env_markers,
        compatible_tags,
        locked_packages,
        favored_packages,
        options,
        env_variables,
    )?;
    let pool = &provider.pool;

    let requirements = requirements.into_iter();

    // Construct the root requirements from the requirements requested by the user.
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
        let dependency_package_name =
            pool.intern_package_name(PypiPackageName::Base(name.clone().into()));
        let version_set_id =
            pool.intern_version_set(dependency_package_name, version_or_url.clone().into());
        root_requirements.push(version_set_id);

        for extra in extras.iter().flatten() {
            let extra: Extra = extra.parse().expect("invalid extra");
            let dependency_package_name = pool
                .intern_package_name(PypiPackageName::Extra(name.clone().into(), extra.clone()));
            let version_set_id =
                pool.intern_version_set(dependency_package_name, version_or_url.clone().into());
            root_requirements.push(version_set_id);
        }
    }

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
                UnsolvableOrCancelled::Cancelled(_) => {
                    Err(miette::miette!("The resolution was cancelled"))
                }
            }
        }
    };

    let mut result = HashMap::new();
    for solvable_id in solvables {
        let pool = solver.pool();
        let solvable = pool.resolve_solvable(solvable_id);
        let name = pool.resolve_package_name(solvable.name_id());
        let PypiVersion::Version(version) = solvable.inner() else {
            unreachable!("urls are not yet supported")
        };

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
                    .copied()
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
