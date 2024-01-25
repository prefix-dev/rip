use super::SDistResolution;
use crate::artifacts::SDist;
use crate::artifacts::Wheel;
use crate::index::PackageDb;
use crate::python_env::WheelTags;
use crate::resolve::{PinnedPackage, ResolveOptions};
use crate::types::{
    Artifact, ArtifactInfo, ArtifactName, Extra, NormalizedPackageName, PackageName,
};
use crate::wheel_builder::WheelBuilder;
use elsa::FrozenMap;
use itertools::Itertools;
use miette::IntoDiagnostic;
use pep440_rs::{Operator, Version, VersionSpecifier, VersionSpecifiers};
use pep508_rs::{MarkerEnvironment, Requirement, VersionOrUrl};
use resolvo::{
    Candidates, Dependencies, DependencyProvider, KnownDependencies, NameId, Pool, SolvableId,
    SolverCache, VersionSet,
};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use tokio::runtime::Handle;
use tokio::task;
use url::Url;

#[repr(transparent)]
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
/// This is a wrapper around [`Specifiers`] that implements [`VersionSet`]
pub(crate) struct PypiVersionSet(Option<VersionOrUrl>);

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
pub(crate) enum PypiVersion {
    Version(Version),
    #[allow(dead_code)]
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
pub(crate) enum PypiPackageName {
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
pub(crate) struct PypiDependencyProvider<'db, 'i> {
    pub pool: Pool<PypiVersionSet, PypiPackageName>,
    package_db: &'db PackageDb,
    wheel_builder: WheelBuilder<'db, 'i>,
    markers: &'i MarkerEnvironment,
    compatible_tags: Option<&'i WheelTags>,

    pub cached_artifacts: FrozenMap<SolvableId, Vec<&'db ArtifactInfo>>,

    favored_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
    locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,

    options: &'i ResolveOptions,
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
        options: &'i ResolveOptions,
        env_variables: HashMap<String, String>,
    ) -> miette::Result<Self> {
        let wheel_builder =
            WheelBuilder::new(package_db, markers, compatible_tags, options, env_variables)
                .into_diagnostic()?;

        Ok(Self {
            pool: Pool::new(),
            package_db,
            wheel_builder,
            markers,
            compatible_tags,
            cached_artifacts: Default::default(),
            favored_packages,
            locked_packages,
            options,
        })
    }

    fn filter_candidates<'a>(
        &self,
        artifacts: &'a [ArtifactInfo],
    ) -> Result<Vec<&'a ArtifactInfo>, &'static str> {
        // Filter only artifacts we can work with
        if artifacts.is_empty() {
            // If there are no wheel artifacts, we're just gonna skip it
            return Err("there are no packages available");
        }

        let mut artifacts = artifacts
            .iter()
            .filter(|a| a.filename.version().pre.is_none() && a.filename.version().dev.is_none())
            .collect::<Vec<_>>();

        if artifacts.is_empty() {
            // Skip all prereleases
            return Err("prereleases are not allowed");
        }

        // Filter yanked artifacts
        artifacts.retain(|a| !a.yanked.yanked);
        if artifacts.is_empty() {
            return Err("it is yanked");
        }

        // This should keep only the wheels
        let mut wheels = if self.options.sdist_resolution.allow_wheels() {
            let wheels = artifacts
                .iter()
                .filter(|a| a.is::<Wheel>())
                .cloned()
                .collect::<Vec<_>>();

            if !self.options.sdist_resolution.allow_sdists() && wheels.is_empty() {
                return Err("there are no wheels available");
            }

            wheels
        } else {
            vec![]
        };

        // Extract sdists
        let mut sdists = if self.options.sdist_resolution.allow_sdists() {
            let mut sdists = artifacts
                .iter()
                .filter(|a| a.is::<SDist>())
                .cloned()
                .collect::<Vec<_>>();

            if wheels.is_empty() && sdists.is_empty() {
                if self.options.sdist_resolution.allow_wheels() {
                    return Err("there are no wheels or sdists");
                } else {
                    return Err("there are no sdists");
                }
            }

            sdists.retain(|a| {
                a.filename
                    .as_sdist()
                    .is_some_and(|f| f.format.is_supported())
            });

            if wheels.is_empty() && sdists.is_empty() {
                return Err("none of the sdists formats are supported");
            }

            sdists
        } else {
            vec![]
        };

        // Filter based on compatibility
        if self.options.sdist_resolution.allow_wheels() {
            if let Some(compatible_tags) = self.compatible_tags {
                wheels.retain(|artifact| match &artifact.filename {
                    ArtifactName::Wheel(wheel_name) => wheel_name
                        .all_tags_iter()
                        .any(|t| compatible_tags.is_compatible(&t)),
                    ArtifactName::SDist(_) => false,
                });

                // Sort the artifacts from most compatible to least compatible, this ensures that we
                // check the most compatible artifacts for dependencies first.
                // this only needs to be done for wheels
                wheels.sort_by_cached_key(|a| {
                    -a.filename
                        .as_wheel()
                        .expect("only wheels are considered")
                        .all_tags_iter()
                        .filter_map(|tag| compatible_tags.compatibility(&tag))
                        .max()
                        .unwrap_or(0)
                });
            }

            if !self.options.sdist_resolution.allow_sdists() && wheels.is_empty() {
                return Err(
                    "none of the artifacts are compatible with the Python interpreter or glibc version",
                );
            }

            if wheels.is_empty() && sdists.is_empty() {
                return Err("none of the artifacts are compatible with the Python interpreter or glibc version and there are no supported sdists");
            }
        }

        // Append these together
        wheels.append(&mut sdists);
        let artifacts = wheels;

        if artifacts.is_empty() {
            return Err("there are no supported artifacts");
        }

        Ok(artifacts)
    }

    fn solvable_has_artifact_type<S: Artifact>(&self, solvable_id: SolvableId) -> bool {
        self.cached_artifacts
            .get(&solvable_id)
            .unwrap_or(&[])
            .iter()
            .any(|a| a.is::<S>())
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
            // First sort the solvables based on the artifact types we have available for them and
            // whether some of them are preferred. If one artifact type is preferred over another
            // we sort those versions above the others even if the versions themselves are lower.
            if matches!(self.options.sdist_resolution, SDistResolution::PreferWheels) {
                let a_has_wheels = self.solvable_has_artifact_type::<Wheel>(a);
                let b_has_wheels = self.solvable_has_artifact_type::<Wheel>(b);
                match (a_has_wheels, b_has_wheels) {
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    _ => {}
                }
            } else if matches!(self.options.sdist_resolution, SDistResolution::PreferSDists) {
                let a_has_sdists = self.solvable_has_artifact_type::<SDist>(a);
                let b_has_sdists = self.solvable_has_artifact_type::<SDist>(b);
                match (a_has_sdists, b_has_sdists) {
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    _ => {}
                }
            }

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

        let mut dependencies = KnownDependencies::default();

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
            let error = self.pool.intern_string(format!(
                "there are no artifacts available for {}={}",
                package_name, package_version
            ));
            return Dependencies::Unknown(error);
        }

        let Some((_, metadata)) = task::block_in_place(|| {
            // First try getting wheels
            Handle::current()
                .block_on(
                    self.package_db
                        .get_metadata(artifacts, Some(&self.wheel_builder)),
                )
                .unwrap()
        }) else {
            let error = self.pool.intern_string(format!("could not find metadata for any sdist or wheel for this package. No metadata could be extracted for the following available artifacts:\n{}",
                                                        artifacts.iter().format_with("\n", |a, f| f(&format_args!("\t- {}", a.filename)))));
            return Dependencies::Unknown(error);
        };

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

        Dependencies::Known(dependencies)
    }
}
