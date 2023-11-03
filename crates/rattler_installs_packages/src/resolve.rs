//! This module contains the [`resolve`] function which is used
//! to make the PyPI ecosystem compatible with the [`resolvo`] crate.
//!
//! To use this enable the `resolve` feature.
//! Note that this module can also serve an example to integrate an alternate packaging system
//! with [`resolvo`].
//!
//! See the `rip_bin` crate for an example of how to use the [`resolve`] function in the: [RIP Repo](https://github.com/prefix-dev/rip)
use crate::tags::WheelTags;
use crate::wheel::Wheel;
use crate::{
    ArtifactInfo, ArtifactName, Extra, NormalizedPackageName, PackageDb, PackageName, Requirement,
    Version,
};
use elsa::FrozenMap;
use pep440_rs::{Operator, VersionSpecifier, VersionSpecifiers};
use pep508_rs::{MarkerEnvironment, VersionOrUrl};
use resolvo::{
    Candidates, DefaultSolvableDisplay, Dependencies, DependencyProvider, NameId, Pool, SolvableId,
    Solver, SolverCache, VersionSet,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use tokio::runtime::Handle;
use tokio::task;
use url::Url;

#[repr(transparent)]
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
/// This is a wrapper around [`Specifiers`] that implements [`VersionSet`]
struct PypiVersionSet(Option<VersionOrUrl>);

impl From<Option<VersionOrUrl>> for PypiVersionSet {
    fn from(value: Option<VersionOrUrl>) -> Self {
        Self(value)
    }
}

impl Display for PypiVersionSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            None => write!(f, "*"),
            Some(VersionOrUrl::Url(url)) => write!(f, "{url}"),
            Some(VersionOrUrl::VersionSpecifier(spec)) => write!(f, "{spec}"),
        }
    }
}

/// This is a wrapper around [`Version`] that serves a version
/// within the [`PypiVersionSet`] version set.
#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
#[allow(dead_code)]
enum PypiVersion {
    Version(Version),
    Url(Url),
}

impl VersionSet for PypiVersionSet {
    type V = PypiVersion;

    fn contains(&self, v: &Self::V) -> bool {
        match (self.0.as_ref(), v) {
            (Some(VersionOrUrl::Url(a)), PypiVersion::Url(b)) => a == b,
            (Some(VersionOrUrl::VersionSpecifier(spec)), PypiVersion::Version(v)) => {
                spec.contains(v)
            }
            (None, _) => true,
            _ => false,
        }
    }
}

impl Display for PypiVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PypiVersion::Version(v) => write!(f, "{v}"),
            PypiVersion::Url(u) => write!(f, "{u}"),
        }
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
struct PypiDependencyProvider<'db, 'i> {
    pool: Pool<PypiVersionSet, PypiPackageName>,
    package_db: &'db PackageDb,
    markers: &'i MarkerEnvironment,
    compatible_tags: Option<&'i WheelTags>,

    cached_artifacts: FrozenMap<SolvableId, Vec<&'db ArtifactInfo>>,

    favored_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
    locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
}

impl<'db, 'i> PypiDependencyProvider<'db, 'i> {
    /// Creates a new PypiDependencyProvider
    /// for use with the [`resolvo`] crate
    pub fn new(
        package_db: &'db PackageDb,
        markers: &'i MarkerEnvironment,
        compatible_tags: Option<&'i WheelTags>,
        locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
        favored_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
    ) -> miette::Result<Self> {
        Ok(Self {
            pool: Pool::new(),
            package_db,
            markers,
            compatible_tags,
            cached_artifacts: Default::default(),
            favored_packages,
            locked_packages,
        })
    }

    fn filter_candidates<'a>(
        &self,
        artifacts: &'a [ArtifactInfo],
    ) -> Result<Vec<&'a ArtifactInfo>, &'static str> {
        let mut artifacts = artifacts
            .iter()
            .filter(|a| a.filename.version().pre.is_none() && a.filename.version().dev.is_none())
            .collect::<Vec<_>>();

        if artifacts.is_empty() {
            // Skip all prereleases
            return Err("prereleases are not allowed");
        }

        // Filter only artifacts we can work with
        artifacts.retain(|a| a.is::<Wheel>());
        if artifacts.is_empty() {
            // If there are no wheel artifacts, we're just gonna skip it
            return Err("there are no wheels available");
        }

        // Filter yanked artifacts
        artifacts.retain(|a| !a.yanked.yanked);
        if artifacts.is_empty() {
            return Err("it is yanked");
        }

        // Filter artifacts that are incompatible with the python version
        artifacts.retain(|artifact| {
            if let Some(requires_python) = artifact.requires_python.as_ref() {
                if !requires_python.contains(&self.markers.python_full_version.version) {
                    return false;
                }
            }
            true
        });

        if artifacts.is_empty() {
            return Err("none of the artifacts are compatible with the Python interpreter");
        }

        // Filter based on compatibility
        if let Some(compatible_tags) = self.compatible_tags {
            artifacts.retain(|artifact| match &artifact.filename {
                ArtifactName::Wheel(wheel_name) => wheel_name
                    .all_tags_iter()
                    .any(|t| compatible_tags.is_compatible(&t)),
                ArtifactName::SDist(_) => unreachable!("sdists have already been filtered"),
            });

            // Sort the artifacts from most compatible to least compatible, this ensures that we
            // check the most compatible artifacts for dependencies first.
            artifacts.sort_by_cached_key(|a| {
                -a.filename
                    .as_wheel()
                    .expect("only wheels are considered")
                    .all_tags_iter()
                    .filter_map(|tag| compatible_tags.compatibility(&tag))
                    .max()
                    .unwrap_or(0)
            });
        }

        if artifacts.is_empty() {
            return Err(
                "none of the artifacts are compatible with the Python interpreter or glibc version",
            );
        }

        Ok(artifacts)
    }
}

impl<'p> DependencyProvider<PypiVersionSet, PypiPackageName>
    for &'p PypiDependencyProvider<'_, '_>
{
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

            match (&solvable_a.inner(), &solvable_b.inner()) {
                // Sort Urls alphabetically
                // TODO: Do better
                (PypiVersion::Url(a), PypiVersion::Url(b)) => a.cmp(b),

                // Prefer Urls over versions
                (PypiVersion::Url(_), PypiVersion::Version(_)) => Ordering::Greater,
                (PypiVersion::Version(_), PypiVersion::Url(_)) => Ordering::Less,

                // Sort versions from highest to lowest
                (PypiVersion::Version(a), PypiVersion::Version(b)) => b.cmp(a),
            }
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
        let locked_package = self.locked_packages.get(package_name.base());
        let favored_package = self.favored_packages.get(package_name.base());
        for (version, artifacts) in artifacts.iter() {
            // Skip this version if a locked or favored version exists for this version. It will be
            // added below.
            if locked_package.map(|p| &p.version) == Some(version)
                || favored_package.map(|p| &p.version) == Some(version)
            {
                continue;
            }

            // Add the solvable
            let solvable_id = self
                .pool
                .intern_solvable(name, PypiVersion::Version(version.clone()));
            candidates.candidates.push(solvable_id);

            // Determine the candidates
            match self.filter_candidates(artifacts) {
                Ok(artifacts) => {
                    self.cached_artifacts.insert(solvable_id, artifacts);
                }
                Err(reason) => {
                    candidates
                        .excluded
                        .push((solvable_id, self.pool.intern_string(reason)));
                }
            }
        }

        // Add a locked dependency
        if let Some(locked) = self.locked_packages.get(package_name.base()) {
            let solvable_id = self
                .pool
                .intern_solvable(name, PypiVersion::Version(locked.version.clone()));
            candidates.candidates.push(solvable_id);
            candidates.locked = Some(solvable_id);
            self.cached_artifacts
                .insert(solvable_id, locked.artifacts.clone());
        }

        // Add a favored dependency
        if let Some(favored) = self.favored_packages.get(package_name.base()) {
            let solvable_id = self
                .pool
                .intern_solvable(name, PypiVersion::Version(favored.version.clone()));
            candidates.candidates.push(solvable_id);
            candidates.favored = Some(solvable_id);
            self.cached_artifacts
                .insert(solvable_id, favored.artifacts.clone());
        }

        Some(candidates)
    }

    fn get_dependencies(&self, solvable_id: SolvableId) -> Dependencies {
        let solvable = self.pool.resolve_solvable(solvable_id);
        let package_name = self.pool.resolve_package_name(solvable.name_id());
        let PypiVersion::Version(package_version) = solvable.inner() else {
            unimplemented!("cannot get dependencies of wheels by url yet")
        };

        tracing::info!(
            "obtaining dependency information from {}={}",
            package_name,
            package_version
        );

        let mut dependencies = Dependencies::default();

        // Add a dependency to the base dependency when we have an extra
        // So that we have a connection to the base package
        if let PypiPackageName::Extra(package_name, _) = package_name {
            let base_name_id = self
                .pool
                .lookup_package_name(&PypiPackageName::Base(package_name.clone()))
                .expect("base package not found while resolving extra");
            let specifiers = VersionSpecifiers::from_iter([VersionSpecifier::new(
                Operator::ExactEqual,
                package_version.clone(),
                false,
            )
            .expect("failed to construct equality version specifier")]);
            let version_set_id = self.pool.intern_version_set(
                base_name_id,
                Some(VersionOrUrl::VersionSpecifier(specifiers)).into(),
            );
            dependencies.requirements.push(version_set_id);
        }

        // Retrieve the artifacts that are applicable for this version
        let artifacts = self
            .cached_artifacts
            .get(&solvable_id)
            .expect("the artifacts must already have been cached");

        // If there are no artifacts we can stop here
        if artifacts.is_empty() {
            return dependencies;
        }

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
                let specifiers = VersionSpecifiers::from_iter([VersionSpecifier::new(
                    Operator::ExactEqual,
                    package_version.clone(),
                    false,
                )
                .expect("failed to construct equality version specifier")]);
                let version_set_id = self.pool.intern_version_set(
                    extra_name_id,
                    Some(VersionOrUrl::VersionSpecifier(specifiers)).into(),
                );
                dependencies.constrains.push(version_set_id);
            }
        }

        let extras = package_name
            .extra()
            .into_iter()
            .map(|e| e.as_str())
            .collect::<Vec<_>>();
        for requirement in metadata.requires_dist {
            // Evaluate environment markers
            if let Some(markers) = requirement.marker.as_ref() {
                if !markers.evaluate(self.markers, &extras) {
                    continue;
                }
            }

            // Add the dependency to the pool
            let Requirement {
                name,
                version_or_url,
                extras,
                ..
            } = requirement;

            let name = PackageName::from_str(&name).expect("invalid package name");
            let dependency_name_id = self
                .pool
                .intern_package_name(PypiPackageName::Base(name.clone().into()));
            let version_set_id = self
                .pool
                .intern_version_set(dependency_name_id, version_or_url.clone().into());
            dependencies.requirements.push(version_set_id);

            // Add a unique package for each extra/optional dependency
            for extra in extras.into_iter().flatten() {
                let extra = Extra::from_str(&extra).expect("invalid extra name");
                let dependency_name_id = self
                    .pool
                    .intern_package_name(PypiPackageName::Extra(name.clone().into(), extra));
                let version_set_id = self
                    .pool
                    .intern_version_set(dependency_name_id, version_or_url.clone().into());
                dependencies.requirements.push(version_set_id);
            }
        }

        dependencies
    }
}

/// Represents a single locked down distribution (python package) after calling [`resolve`].
#[derive(Debug)]
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
pub async fn resolve<'db>(
    package_db: &'db PackageDb,
    requirements: impl IntoIterator<Item = &Requirement>,
    env_markers: &MarkerEnvironment,
    compatible_tags: Option<&WheelTags>,
    locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
    favored_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
) -> miette::Result<Vec<PinnedPackage<'db>>> {
    // Construct a provider
    let provider = PypiDependencyProvider::new(
        package_db,
        env_markers,
        compatible_tags,
        locked_packages,
        favored_packages,
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
            return Err(miette::miette!(
                "{}",
                e.display_user_friendly(&solver, &DefaultSolvableDisplay)
                    .to_string()
                    .trim()
            ))
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
