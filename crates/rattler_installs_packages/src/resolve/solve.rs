use crate::index::PackageDb;
use crate::python_env::WheelTags;
use crate::resolve::dependency_provider::PypiDependencyProvider;
use crate::resolve::pypi_version_types::PypiVersion;
use crate::types::PackageName;
use crate::{types::ArtifactInfo, types::Extra, types::NormalizedPackageName};
use elsa::FrozenMap;
use pep440_rs::Version;
use pep508_rs::{MarkerEnvironment, Requirement, VersionOrUrl};
use resolvo::{DefaultSolvableDisplay, Pool, Solver, UnsolvableOrCancelled};
use std::collections::HashMap;
use std::str::FromStr;
use url::Url;

use crate::resolve::pypi_version_types::{PypiPackageName, PypiVersionSet};
use crate::resolve::solve_options::ResolveOptions;
use std::collections::HashSet;
use std::convert::identity;
use std::ops::Deref;
use std::sync::Arc;

/// Represents a single locked down distribution (python package) after calling [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedPackage {
    /// The name of the package
    pub name: NormalizedPackageName,

    /// The selected version
    pub version: Version,

    /// The possible direct URL for it
    pub url: Option<Url>,

    /// The extras that where selected either by the user or as part of the resolution.
    pub extras: HashSet<Extra>,

    /// The applicable artifacts for this package. These have been ordered by compatibility if
    /// `compatible_tags` have been provided to the solver.
    ///
    /// This list may be empty if the package was locked or favored.
    pub artifacts: Vec<Arc<ArtifactInfo>>,
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
pub async fn resolve(
    package_db: Arc<PackageDb>,
    requirements: impl IntoIterator<Item = &Requirement>,
    env_markers: Arc<MarkerEnvironment>,
    compatible_tags: Option<Arc<WheelTags>>,
    options: ResolveOptions,
) -> miette::Result<Vec<PinnedPackage>> {
    let requirements: Vec<_> = requirements.into_iter().cloned().collect();
    tokio::task::spawn_blocking(move || {
        resolve_inner(
            package_db,
            &requirements,
            env_markers,
            compatible_tags,
            options,
        )
    })
    .await
    .map_or_else(
        |e| match e.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => Err(miette::miette!("the operation was cancelled")),
        },
        identity,
    )
}

fn resolve_inner<'r>(
    package_db: Arc<PackageDb>,
    requirements: impl IntoIterator<Item = &'r Requirement>,
    env_markers: Arc<MarkerEnvironment>,
    compatible_tags: Option<Arc<WheelTags>>,
    options: ResolveOptions,
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
        name_to_url,
        options,
    )?;

    // Invoke the solver to get a solution to the requirements
    let mut solver = Solver::new(&provider).with_runtime(tokio::runtime::Handle::current());
    let solvables = match solver.solve(root_requirements) {
        Ok(solvables) => solvables,
        Err(e) => {
            return match e {
                UnsolvableOrCancelled::Unsolvable(problem) => Err(miette::miette!(
                    "{}",
                    problem
                        .display_user_friendly(
                            &solver,
                            solver.pool.clone(),
                            &DefaultSolvableDisplay
                        )
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
        let solvable = solver.pool.resolve_solvable(solvable_id);
        let name = solver.pool.resolve_package_name(solvable.name_id());
        let version = solvable.inner();

        let artifacts: Vec<_> = provider
            .cached_artifacts
            .get(&solvable_id)
            .into_iter()
            .flatten()
            .cloned()
            .collect();

        let (version, url) = match version {
            PypiVersion::Version { version, .. } => (version.clone(), None),
            PypiVersion::Url(url) => {
                // artifacts retrieved by url have only one artifact and one possible version
                let info = artifacts
                    .first()
                    .expect("no artifacts found for direct_url artifact");
                (info.filename.version(), Some(url.clone()))
            }
        };

        // Get the entry in the result
        let entry = result
            .entry(name.base().clone())
            .or_insert_with(|| PinnedPackage {
                name: name.base().clone(),
                version,
                url,
                artifacts,
                extras: Default::default(),
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
