use rattler_installs_packages::requirement::Requirement;
use rattler_installs_packages::{
    CompareOp, Extra, NormalizedPackageName, PackageDb, Specifier, Specifiers, Version, Wheel,
};
use resolvo::{
    Candidates, Dependencies, DependencyProvider, NameId, Pool, SolvableId, SolverCache, VersionSet,
};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use tokio::runtime::Handle;
use tokio::task;

#[repr(transparent)]
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct PypiVersionSet(Specifiers);

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
pub struct PypiVersion(pub Version);

impl VersionSet for PypiVersionSet {
    type V = PypiVersion;

    fn contains(&self, v: &Self::V) -> bool {
        match self.0.satisfied_by(&v.0) {
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
pub enum PypiPackageName {
    Base(NormalizedPackageName),
    Extra(NormalizedPackageName, Extra),
}

impl PypiPackageName {
    pub fn base(&self) -> &NormalizedPackageName {
        match self {
            PypiPackageName::Base(normalized) => normalized,
            PypiPackageName::Extra(normalized, _) => normalized,
        }
    }

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

pub struct PypiDependencyProvider {
    pool: Pool<PypiVersionSet, PypiPackageName>,
    package_db: PackageDb,
}

impl PypiDependencyProvider {
    pub fn new(package_db: PackageDb) -> Self {
        Self {
            pool: Pool::new(),
            package_db,
        }
    }
}

impl DependencyProvider<PypiVersionSet, PypiPackageName> for PypiDependencyProvider {
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
        tracing::info!("Fetching metadata for {}", package_name);

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
        for (version, artifacts) in artifacts.iter() {
            // Filter only artifacts we can work with
            let available_artifacts = artifacts
                .iter()
                // We are only interested in wheels
                .filter(|a| a.is::<Wheel>())
                // TODO: How to filter prereleases correctly?
                .filter(|a| {
                    a.filename.version().pre.is_none() && a.filename.version().dev.is_none()
                })
                .collect::<Vec<_>>();

            // Check if there are wheel artifacts for this version
            if available_artifacts.is_empty() {
                // If there are no wheel artifacts, we're just gonna skip it
                tracing::warn!("No available wheel artifact {package_name} {version} (skipping)");
                continue;
            }

            // Filter yanked artifacts
            let non_yanked_artifacts = artifacts
                .iter()
                .filter(|a| !a.yanked.yanked)
                .collect::<Vec<_>>();

            if non_yanked_artifacts.is_empty() {
                tracing::info!("{package_name} {version} was yanked (skipping)");
                continue;
            }
            let solvable_id = self
                .pool
                .intern_solvable(name, PypiVersion(version.clone()));
            candidates.candidates.push(solvable_id);
        }
        Some(candidates)
    }

    fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        let solvable = self.pool.resolve_solvable(solvable);
        let package_name = self.pool.resolve_package_name(solvable.name_id());

        // TODO: https://peps.python.org/pep-0508/#environment-markers
        let env = HashMap::from_iter([
            // TODO: We should add some proper values here.
            // See: https://peps.python.org/pep-0508/#environment-markers
            ("os_name", ""),
            ("sys_platform", ""),
            ("platform_machine", ""),
            ("platform_python_implementation", ""),
            ("platform_release", ""),
            ("platform_system", ""),
            ("platform_version", ""),
            ("python_version", "3.9"),
            ("python_full_version", ""),
            ("implementation_name", ""),
            ("implementation_version", ""),
            (
                "extra",
                package_name.extra().map(|e| e.as_str()).unwrap_or(""),
            ),
        ]);
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

        let result = task::block_in_place(move || {
            Handle::current().block_on(
                self.package_db
                    .available_artifacts(package_name.base().clone()),
            )
        });

        let artifacts_per_version = match result {
            Ok(artifacts) => artifacts,
            Err(e) => {
                tracing::error!("failed to fetch artifacts of '{package_name}': {e:?}, skipping..");
                return dependencies;
            }
        };

        let artifacts = artifacts_per_version
            .get(&solvable.inner().0.clone())
            .expect("strange, no artificats are available");

        // Filter yanked artifacts
        let non_yanked_artifacts = artifacts
            .iter()
            .filter(|a| !a.yanked.yanked)
            .collect::<Vec<_>>();

        if non_yanked_artifacts.is_empty() {
            panic!("no artifacts are available after removing yanked artifacts");
        }

        let (_, metadata) = task::block_in_place(|| {
            Handle::current()
                .block_on(
                    self.package_db
                        .get_metadata::<Wheel, _>(&non_yanked_artifacts),
                )
                .unwrap()
        });

        for requirement in metadata.requires_dist {
            // Evaluate environment markers
            if let Some(env_marker) = &requirement.env_marker_expr {
                if !env_marker.eval(&env).unwrap() {
                    // tracing::info!("skipping dependency {requirement}");
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
