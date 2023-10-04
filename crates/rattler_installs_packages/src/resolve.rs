//! This module contains the [`resolve`] function which is used
//! to make the PyPI ecosystem compatible with the [`resolvo`] crate.
//!
//! To use this enable the `resolve` feature.
//! Note that this module can also serve an example to integrate an alternate packaging system
//! with [`resolvo`].
//!
//! See the `rip_bin` crate for an example of how to use the [`resolve`] function in the: [RIP Repo](https://github.com/prefix-dev/rip)
use crate::env_markers::Pep508EnvMakers;
use crate::marker::Env;
use crate::tags::WheelTags;
use crate::{
    ArtifactInfo, ArtifactName, CompareOp, Extra, NormalizedPackageName, PackageDb, PackageName,
    Requirement, Specifier, Specifiers, UserRequirement, Version, Wheel,
};
use elsa::FrozenMap;
use itertools::Itertools;
use resolvo::{
    Candidates, DefaultSolvableDisplay, Dependencies, DependencyProvider, NameId, Pool, SolvableId,
    Solver, SolverCache, VersionSet,
};
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use tokio::runtime::Handle;
use tokio::task;

#[repr(transparent)]
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
/// This is a wrapper around [`Specifiers`] that implements [`VersionSet`]
struct PypiVersionSet(Specifiers);

impl From<Specifiers> for PypiVersionSet {
    fn from(value: Specifiers) -> Self {
        Self(value)
    }
}

impl Display for PypiVersionSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
/// This is a wrapper around [`Version`] that serves a version
/// within the [`PypiVersionSet`] version set.
struct PypiVersion(pub Version);

impl VersionSet for PypiVersionSet {
    type V = PypiVersion;

    fn contains(&self, v: &Self::V) -> bool {
        match self.0.satisfied_by(&v.0, true) {
            Err(e) => {
                tracing::error!("failed to determine if '{}' contains '{}': {e}", &self.0, v);
                false
            }
            Ok(result) => result,
        }
    }
}

impl Display for PypiVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0)
    }
}

#[derive(PartialEq, Eq, Hash, Clone)]
/// This can either be a base package name or with an extra
/// this is used to support optional dependencies
pub enum PypiPackageName {
    /// Regular dependency
    Base(NormalizedPackageName),
    /// Optional dependency
    Extra(NormalizedPackageName, Extra),
}

impl PypiPackageName {
    /// Returns the actual package (normalized) name without the extra
    pub fn base(&self) -> &NormalizedPackageName {
        match self {
            PypiPackageName::Base(normalized) => normalized,
            PypiPackageName::Extra(normalized, _) => normalized,
        }
    }

    /// Retrieves the extra if it is available
    pub fn extra(&self) -> Option<&Extra> {
        match self {
            PypiPackageName::Base(_) => None,
            PypiPackageName::Extra(_, e) => Some(e),
        }
    }
}

impl Display for PypiPackageName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PypiPackageName::Base(name) => write!(f, "{}", name),
            PypiPackageName::Extra(name, extra) => write!(f, "{}[{}]", name, extra.as_str()),
        }
    }
}

/// This is a [`DependencyProvider`] for PyPI packages
struct PypiDependencyProvider<'db, E> {
    pool: Pool<PypiVersionSet, PypiPackageName>,
    package_db: &'db PackageDb,
    env_markers: E,
    python_version: Version,
    compatible_tags: Option<&'db WheelTags>,

    cached_artifacts: FrozenMap<SolvableId, Vec<&'db ArtifactInfo>>,
}

impl<'db, E: Env> PypiDependencyProvider<'db, E> {
    /// Creates a new PypiDependencyProvider
    /// for use with the [`resolvo`] crate
    pub fn new(
        package_db: &'db PackageDb,
        env_markers: E,
        compatible_tags: Option<&'db WheelTags>,
    ) -> miette::Result<Self> {
        let version = env_markers
            .get_marker_var("python_full_version")
            .ok_or(miette::miette!(
                "missing 'python_full_version' environment marker variable"
            ))?
            .parse()
            .map_err(|e| miette::miette!("failed to parse 'python_full_version': {e}"))?;

        Ok(Self {
            pool: Pool::new(),
            package_db,
            env_markers,
            python_version: version,
            compatible_tags,
            cached_artifacts: Default::default(),
        })
    }
}

impl<E: Env> DependencyProvider<PypiVersionSet, PypiPackageName> for PypiDependencyProvider<'_, E> {
    fn pool(&self) -> &Pool<PypiVersionSet, PypiPackageName> {
        &self.pool
    }

    fn sort_candidates(
        &self,
        solver: &SolverCache<PypiVersionSet, PypiPackageName, Self>,
        solvables: &mut [SolvableId],
    ) {
        solvables.sort_by(|&a, &b| {
            let solvable_a = solver.pool().resolve_solvable(a);
            let solvable_b = solver.pool().resolve_solvable(b);

            let a = &solvable_a.inner().0;
            let b = &solvable_b.inner().0;

            // Sort in reverse order from highest to lowest.
            b.cmp(a)
        })
    }

    fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let package_name = self.pool.resolve_package_name(name);
        tracing::info!("collecting {}", package_name);

        // Get all the metadata for this package
        let result = task::block_in_place(move || {
            Handle::current().block_on(
                self.package_db
                    .available_artifacts(package_name.base().clone()),
            )
        });
        let artifacts = match result {
            Ok(artifacts) => artifacts,
            Err(err) => {
                tracing::error!(
                    "failed to fetch artifacts of '{package_name}': {err:?}, skipping.."
                );
                return None;
            }
        };
        let mut candidates = Candidates::default();
        let mut no_wheels = Vec::new();
        let mut incompatible_python = Vec::new();
        let mut incompatible_tags = Vec::new();
        for (version, artifacts) in artifacts.iter() {
            let mut artifacts = artifacts
                .iter()
                .filter(|a| {
                    a.filename.version().pre.is_none() && a.filename.version().dev.is_none()
                })
                .collect::<Vec<_>>();

            if artifacts.is_empty() {
                // Skip all prereleases
                continue;
            }

            // Filter only artifacts we can work with
            artifacts.retain(|a| a.is::<Wheel>());
            if artifacts.is_empty() {
                // If there are no wheel artifacts, we're just gonna skip it
                no_wheels.push(version);
                continue;
            }

            // Filter yanked artifacts
            artifacts.retain(|a| !a.yanked.yanked);
            if artifacts.is_empty() {
                continue;
            }

            // Filter artifacts that are incompatible with the python version
            artifacts.retain(|artifact| {
                if let Some(requires_python) = artifact.requires_python.as_ref() {
                    let python_specifier: Specifiers = requires_python
                        .parse()
                        .expect("invalid requires_python specifier");
                    if !python_specifier
                        .satisfied_by(&self.python_version, true)
                        .expect("failed to determine satisfiability of requires_python specifier")
                    {
                        return false;
                    }
                }
                true
            });

            if artifacts.is_empty() {
                incompatible_python.push(version);
                continue;
            }

            // Filter based on compatibility
            if let Some(compatible_tags) = self.compatible_tags {
                artifacts.retain(|artifact| match &artifact.filename {
                    ArtifactName::Wheel(wheel_name) => wheel_name
                        .all_tags_iter()
                        .any(|t| compatible_tags.is_compatible(&t)),
                    ArtifactName::SDist(_) => unreachable!("sdists have already been filtered"),
                });
            }

            if artifacts.is_empty() {
                incompatible_tags.push(version);
                continue;
            }

            let solvable_id = self
                .pool
                .intern_solvable(name, PypiVersion(version.clone()));
            candidates.candidates.push(solvable_id);
            self.cached_artifacts.insert(solvable_id, artifacts);
        }

        // Print some information about skipped packages
        if !no_wheels.is_empty() && package_name.extra().is_none() {
            tracing::warn!(
                "Not considering {} {} because there are no wheel artifacts available",
                package_name,
                no_wheels.iter().format(", "),
            );
        }

        if !incompatible_python.is_empty() && package_name.extra().is_none() {
            tracing::warn!(
                "Not considering {} {} because none of the artifacts are compatible with Python {}",
                package_name,
                incompatible_python.iter().format(", "),
                &self.python_version
            );
        }

        if !incompatible_tags.is_empty() && package_name.extra().is_none() {
            tracing::warn!(
                "Not considering {} {} because none of the artifacts are compatible with the Python interpreter",
                package_name,
                incompatible_tags.iter().format(", "),
            );
        }

        Some(candidates)
    }

    fn get_dependencies(&self, solvable_id: SolvableId) -> Dependencies {
        let solvable = self.pool.resolve_solvable(solvable_id);
        let package_name = self.pool.resolve_package_name(solvable.name_id());

        tracing::info!(
            "obtaining dependency information from {}={}",
            package_name,
            solvable.inner()
        );

        let env = ExtraEnv {
            env: &self.env_markers,
            extra: package_name.extra().map(|e| e.as_str()).unwrap_or(""),
        };
        let mut dependencies = Dependencies::default();

        // Add a dependency to the base dependency when we have an extra
        // So that we have a connection to the base package
        if let PypiPackageName::Extra(package_name, _) = package_name {
            let base_name_id = self
                .pool
                .lookup_package_name(&PypiPackageName::Base(package_name.clone()))
                .expect("base package not found while resolving extra");
            let specifiers = Specifiers(vec![Specifier {
                op: CompareOp::Equal,
                value: solvable.inner().0.to_string(),
            }]);
            let version_set_id = self
                .pool
                .intern_version_set(base_name_id, specifiers.into());
            dependencies.requirements.push(version_set_id);
        }

        // Retrieve the artifacts that are applicable for this version
        let artifacts = self
            .cached_artifacts
            .get(&solvable_id)
            .expect("the artifacts must already have been cached");
        let (_, metadata) = task::block_in_place(|| {
            Handle::current()
                .block_on(self.package_db.get_metadata::<Wheel, _>(artifacts))
                .unwrap()
        });

        // Add constraints that restrict that the extra packages are set to the same version.
        if let PypiPackageName::Base(package_name) = package_name {
            // Add constraints on the extras of a package
            for extra in metadata.extras {
                let extra_name_id = self
                    .pool
                    .intern_package_name(PypiPackageName::Extra(package_name.clone(), extra));
                let specifiers = Specifiers(vec![Specifier {
                    op: CompareOp::Equal,
                    value: solvable.inner().0.to_string(),
                }]);
                let version_set_id = self
                    .pool
                    .intern_version_set(extra_name_id, specifiers.into());
                dependencies.constrains.push(version_set_id);
            }
        }

        for requirement in metadata.requires_dist {
            // Evaluate environment markers
            if let Some(env_marker) = &requirement.env_marker_expr {
                if !env_marker.eval(&env).unwrap() {
                    continue;
                }
            }

            // Add the dependency to the pool
            let Requirement {
                name,
                specifiers,
                extras,
                ..
            } = requirement.into_inner();

            let dependency_name_id = self
                .pool
                .intern_package_name(PypiPackageName::Base(name.clone().into()));
            let version_set_id = self
                .pool
                .intern_version_set(dependency_name_id, specifiers.clone().into());
            dependencies.requirements.push(version_set_id);

            // Add a unique package for each extra/optional dependency
            for extra in extras {
                let dependency_name_id = self
                    .pool
                    .intern_package_name(PypiPackageName::Extra(name.clone().into(), extra));
                let version_set_id = self
                    .pool
                    .intern_version_set(dependency_name_id, specifiers.clone().into());
                dependencies.requirements.push(version_set_id);
            }
        }

        dependencies
    }
}

/// Combines an extra env marker with another object that implements [`Env`].
struct ExtraEnv<'e, E> {
    env: &'e E,
    extra: &'e str,
}

impl<'e, E: Env> Env for ExtraEnv<'e, E> {
    fn get_marker_var(&self, var: &str) -> Option<&str> {
        if var == "extra" {
            Some(self.extra)
        } else {
            self.env.get_marker_var(var)
        }
    }
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
/// artifacts are not filtered at all.
pub async fn resolve(
    package_db: &PackageDb,
    requirements: impl IntoIterator<Item = &UserRequirement>,
    env_markers: &Pep508EnvMakers,
    compatible_tags: Option<&WheelTags>,
) -> miette::Result<HashMap<PackageName, (Version, HashSet<Extra>)>> {
    // Construct a provider
    let provider = PypiDependencyProvider::new(package_db, env_markers, compatible_tags)?;
    let pool = provider.pool();

    let requirements = requirements.into_iter();

    // Construct the root requirements from the requirements requested by the user.
    let requirement_count = requirements.size_hint();
    let mut root_requirements =
        Vec::with_capacity(requirement_count.1.unwrap_or(requirement_count.0));
    for Requirement {
        name,
        specifiers,
        extras,
        ..
    } in requirements.map(UserRequirement::as_inner)
    {
        let dependency_package_name =
            pool.intern_package_name(PypiPackageName::Base(name.clone().into()));
        let version_set_id =
            pool.intern_version_set(dependency_package_name, specifiers.clone().into());
        root_requirements.push(version_set_id);

        for extra in extras {
            let dependency_package_name = pool
                .intern_package_name(PypiPackageName::Extra(name.clone().into(), extra.clone()));
            let version_set_id =
                pool.intern_version_set(dependency_package_name, specifiers.clone().into());
            root_requirements.push(version_set_id);
        }
    }

    // Invoke the solver to get a solution to the requirements
    let mut solver = Solver::new(provider);
    let result = solver.solve(root_requirements);

    match result {
        Ok(solvables) => {
            let mut result = HashMap::default();
            for solvable in solvables {
                let pool = solver.pool();
                let solvable = pool.resolve_solvable(solvable);
                let name = pool.resolve_package_name(solvable.name_id());
                let version = solvable.inner();
                match name {
                    PypiPackageName::Base(name) => {
                        result
                            .entry(name.clone().into())
                            .or_insert((version.0.clone(), HashSet::new()));
                    }
                    PypiPackageName::Extra(name, extra) => {
                        let (_, extras) = result
                            .entry(name.clone().into())
                            .or_insert((version.0.clone(), HashSet::new()));
                        extras.insert(extra.clone());
                    }
                }
            }
            Ok(result)
        }
        Err(e) => Err(miette::miette!(
            "{}",
            e.display_user_friendly(&solver, &DefaultSolvableDisplay)
        )),
    }
}
