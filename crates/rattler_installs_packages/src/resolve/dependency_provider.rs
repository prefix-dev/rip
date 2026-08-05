use super::{
    PypiVersion, PypiVersionSet,
    pypi_version_types::PypiPackageName,
    solve_options::{PreReleaseResolution, ResolveOptions, SDistResolution},
};
use crate::{
    artifacts::{SDist, Wheel},
    index::{ArtifactRequest, PackageDb},
    python_env::WheelTags,
    types::{
        ArtifactFromBytes, ArtifactInfo, ArtifactName, Extra, NormalizedPackageName, PackageName,
    },
    wheel_builder::WheelBuilder,
};
use elsa::FrozenMap;
use itertools::Itertools;
use miette::{Diagnostic, MietteDiagnostic};
use parking_lot::Mutex;
use pep508_rs::{ExtraName, MarkerEnvironment, Requirement, VersionOrUrl};
use resolvo::{
    Candidates, ConditionalRequirement, Dependencies, DependencyProvider,
    HintDependenciesAvailable, KnownDependencies, NameId, Requirement as ResolvoRequirement,
    SolvableId, SolverCache, StringId, VersionSetId, utils::Pool,
};
use std::{any::Any, borrow::Borrow, cmp::Ordering, rc::Rc, str::FromStr, sync::Arc};
use thiserror::Error;
use url::Url;

/// This is a [`DependencyProvider`] for PyPI packages
pub(crate) struct PypiDependencyProvider {
    pub pool: Rc<Pool<PypiVersionSet, PypiPackageName>>,
    pub cached_artifacts: FrozenMap<SolvableId, Vec<Arc<ArtifactInfo>>>,
    pub name_to_url: FrozenMap<NormalizedPackageName, String>,
    package_db: Arc<PackageDb>,
    wheel_builder: Arc<WheelBuilder>,
    markers: Arc<MarkerEnvironment>,
    compatible_tags: Option<Arc<WheelTags>>,

    options: ResolveOptions,
    should_cancel_with_value: Mutex<Option<MetadataError>>,

    /// Maps extras to their version set IDs for use in conditions
    /// The key is (package_name, extra_name) and the value is the VersionSetId
    /// that represents "package[extra] is requested"
    /// Boxed because FrozenMap requires Deref types
    extra_conditions: FrozenMap<(NormalizedPackageName, Extra), Box<VersionSetId>>,
}

impl PypiDependencyProvider {
    /// Creates a new PypiDependencyProvider
    /// for use with the [`resolvo`] crate
    pub fn new(
        pool: Pool<PypiVersionSet, PypiPackageName>,
        package_db: Arc<PackageDb>,
        markers: Arc<MarkerEnvironment>,
        compatible_tags: Option<Arc<WheelTags>>,
        name_to_url: FrozenMap<NormalizedPackageName, String>,
        wheel_builder: Arc<WheelBuilder>,
        options: ResolveOptions,
    ) -> miette::Result<Self> {
        Ok(Self {
            pool: Rc::new(pool),
            package_db,
            wheel_builder,
            markers,
            compatible_tags,
            cached_artifacts: Default::default(),
            name_to_url,
            options,
            should_cancel_with_value: Default::default(),
            extra_conditions: Default::default(),
        })
    }

    fn filter_artifact_candidates<'a, A: Borrow<ArtifactInfo>>(
        &self,
        artifacts: &'a [A],
    ) -> Result<Vec<&'a A>, &'static str> {
        // Filter only artifacts we can work with
        if artifacts.is_empty() {
            // If there are no wheel artifacts, we're just gonna skip it
            return Err("there are no packages available");
        }

        let mut artifacts = artifacts.iter().collect::<Vec<_>>();
        // Filter yanked artifacts
        artifacts.retain(|a| !(*a).borrow().yanked.yanked);

        if artifacts.is_empty() {
            return Err("it is yanked");
        }

        // This should keep only the wheels
        let mut wheels = if self.options.sdist_resolution.allow_wheels() {
            let wheels = artifacts
                .iter()
                .copied()
                .filter(|a| (*a).borrow().is::<Wheel>())
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
                .copied()
                .filter(|a| {
                    (*a).borrow().is::<SDist>() || (*a).borrow().filename.as_stree().is_some()
                })
                .collect::<Vec<_>>();

            if wheels.is_empty() && sdists.is_empty() {
                if self.options.sdist_resolution.allow_wheels() {
                    return Err("there are no wheels or sdists");
                } else {
                    return Err("there are no sdists");
                }
            }

            sdists.retain(|a| {
                let ai = (*a).borrow();
                ai.filename
                    .as_sdist()
                    .is_some_and(|f| f.format.is_supported())
                    || ai.filename.as_stree().is_some()
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
            if let Some(compatible_tags) = &self.compatible_tags {
                wheels.retain(|artifact| match &(*artifact).borrow().filename {
                    ArtifactName::Wheel(wheel_name) => wheel_name
                        .all_tags_iter()
                        .any(|t| compatible_tags.is_compatible(&t)),
                    ArtifactName::SDist(_) => false,
                    ArtifactName::STree(_) => false,
                });

                // Sort the artifacts from most compatible to least compatible, this ensures that we
                // check the most compatible artifacts for dependencies first.
                // this only needs to be done for wheels
                wheels.sort_by_cached_key(|a| {
                    -(*a)
                        .borrow()
                        .filename
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
                return Err(
                    "none of the artifacts are compatible with the Python interpreter or glibc version and there are no supported sdists",
                );
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

    fn solvable_has_artifact_type<S: ArtifactFromBytes>(&self, solvable_id: SolvableId) -> bool {
        self.cached_artifacts
            .get(&solvable_id)
            .unwrap_or(&[])
            .iter()
            .any(|a| a.is::<S>())
    }

    /// Acquires a lease to be able to spawn a task
    /// this is used to limit the amount of concurrent tasks
    async fn aquire_lease_to_run(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.options
            .max_concurrent_tasks
            .clone()
            .acquire_owned()
            .await
            .expect("could not acquire semaphore")
    }

    /// Creates or retrieves a condition ID for a given package extra
    /// This is used to make dependencies conditional on extras being selected
    fn extra_condition(
        &self,
        package: &NormalizedPackageName,
        extra: &Extra,
    ) -> resolvo::ConditionId {
        use resolvo::Condition;

        // Create the extra feature name
        let extra_name = PypiPackageName::extra_feature(package.clone(), extra.clone());
        let name_id = self.pool.intern_package_name(extra_name);

        // Check if we already have a condition for this extra
        if let Some(boxed_id) = self.extra_conditions.get(&(package.clone(), extra.clone())) {
            // boxed_id is &Box<VersionSetId>
            // VersionSetId is a tuple struct with one field (u32), access it directly
            // Box<T> derefs to &T automatically, so we can copy the inner value
            let version_set_id = VersionSetId(boxed_id.0);
            return self
                .pool
                .intern_condition(Condition::Requirement(version_set_id));
        }

        // Create a new version set for this extra (any version of the extra feature)
        let version_set_id = self.pool.intern_version_set(
            name_id,
            PypiVersionSet::from_spec(None, &self.options.pre_release_resolution),
        );

        // Cache it for future use
        self.extra_conditions
            .insert((package.clone(), extra.clone()), Box::new(version_set_id));

        // Return the condition
        self.pool
            .intern_condition(Condition::Requirement(version_set_id))
    }
}

#[derive(Debug, Error, Diagnostic, Clone)]
pub(crate) enum MetadataError {
    #[error(
        "Extraction of metadata in case of wheels or building in case of sdists returned no results for following artifacts:\n{0}"
    )]
    NoMetadata(String),

    #[error("No metadata could be extracted for the following available artifacts:\n{artifacts}")]
    ExtractionFailure {
        artifacts: String,
        #[related]
        errors: Vec<MietteDiagnostic>,
    },
}

impl resolvo::Interner for &PypiDependencyProvider {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, solvable: SolvableId) -> impl std::fmt::Display + '_ {
        let solvable = self.pool.resolve_solvable(solvable);
        let name = self.pool.resolve_package_name(solvable.name);
        let version = &solvable.record;
        format!("{name}=={version}")
    }

    fn display_name(&self, name: NameId) -> impl std::fmt::Display + '_ {
        self.pool.resolve_package_name(name).to_string()
    }

    fn display_version_set(&self, version_set: VersionSetId) -> impl std::fmt::Display + '_ {
        let version_set = self.pool.resolve_version_set(version_set);
        version_set.to_string()
    }

    fn display_string(&self, string_id: StringId) -> impl std::fmt::Display + '_ {
        self.pool.resolve_string(string_id)
    }

    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.pool.resolve_version_set_package_name(version_set)
    }

    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.pool.resolve_solvable(solvable).name
    }

    fn version_sets_in_union(
        &self,
        _version_set_union: resolvo::VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        std::iter::empty()
    }

    fn resolve_condition(&self, condition: resolvo::ConditionId) -> resolvo::Condition {
        self.pool.resolve_condition(condition).clone()
    }
}

impl DependencyProvider for &PypiDependencyProvider {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let version_set = self.pool.resolve_version_set(version_set);
        candidates
            .iter()
            .copied()
            .filter(|&candidate| {
                let solvable = self.pool.resolve_solvable(candidate);
                let contains = version_set.contains(&solvable.record);
                if inverse { !contains } else { contains }
            })
            .collect()
    }

    fn should_cancel_with_value(&self) -> Option<Box<dyn Any>> {
        // Supply the error message
        self.should_cancel_with_value
            .lock()
            .as_ref()
            .map(|s| Box::new(s.clone()) as Box<dyn Any>)
    }

    async fn sort_candidates(&self, _: &SolverCache<Self>, solvables: &mut [SolvableId]) {
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

            let solvable_a = self.pool.resolve_solvable(a);
            let solvable_b = self.pool.resolve_solvable(b);

            match (&solvable_a.record, &solvable_b.record) {
                // Sort Urls alphabetically
                // TODO: Do better
                (PypiVersion::Url(a), PypiVersion::Url(b)) => a.cmp(b),

                // Prefer Urls over versions
                (PypiVersion::Url(_), PypiVersion::Version { .. }) => Ordering::Greater,
                (PypiVersion::Version { .. }, PypiVersion::Url(_)) => Ordering::Less,

                // Sort versions from highest to lowest
                (
                    PypiVersion::Version { version: a, .. },
                    PypiVersion::Version { version: b, .. },
                ) => b.cmp(a),

                // Extras don't need special sorting - they're always unique
                (PypiVersion::Extra { .. }, PypiVersion::Extra { .. }) => Ordering::Equal,

                // Put extras at the end
                (PypiVersion::Extra { .. }, _) => Ordering::Less,
                (_, PypiVersion::Extra { .. }) => Ordering::Greater,
            }
        })
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let package_name = self.pool.resolve_package_name(name);
        tracing::info!("collecting {}", package_name);

        // Handle extra features specially - they are virtual solvables
        if let PypiPackageName::ExtraFeature(base_name, extra) = package_name {
            // For extras, we create a single virtual solvable
            let extra_solvable = self.pool.intern_solvable(
                name,
                PypiVersion::Extra {
                    package: base_name.clone(),
                    extra: extra.clone(),
                },
            );
            return Some(Candidates {
                candidates: vec![extra_solvable],
                favored: None,
                locked: None,
                excluded: Vec::new(),
                hint_dependencies_available: HintDependenciesAvailable::All,
            });
        }

        // check if we have URL variant for this name
        let url_version = self.name_to_url.get(package_name.base_package());

        let request = if let Some(url) = url_version {
            ArtifactRequest::DirectUrl {
                name: package_name.base_package().clone(),
                url: Url::from_str(url).expect("cannot parse back url"),
                wheel_builder: self.wheel_builder.clone(),
            }
        } else {
            ArtifactRequest::FromIndex(package_name.base_package().clone())
        };

        let lease = self.aquire_lease_to_run().await;
        let result: Result<_, miette::Report> = tokio::spawn({
            let package_db = self.package_db.clone();
            async move {
                let result = package_db.available_artifacts(request).await?.clone();
                drop(lease);
                Ok(result)
            }
        })
        .await
        .expect("cancelled");

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
        let locked_package = self
            .options
            .locked_packages
            .get(package_name.base_package());
        let favored_package = self
            .options
            .favored_packages
            .get(package_name.base_package());

        let should_package_allow_prerelease = match &self.options.pre_release_resolution {
            PreReleaseResolution::Disallow => false,
            PreReleaseResolution::AllowIfNoOtherVersionsOrEnabled { allow_names } => {
                if allow_names.contains(&package_name.base_package().to_string()) {
                    true
                } else {
                    // check if we _only_ have prereleases for this name (if yes, also allow them)
                    artifacts
                        .iter()
                        .all(|(version, _)| version.any_prerelease())
                }
            }
            PreReleaseResolution::Allow => true,
        };

        for (artifact_version, artifacts) in artifacts.iter() {
            // Skip this version if a locked or favored version exists for this version. It will be
            // added below.

            match artifact_version {
                PypiVersion::Url(url) => {
                    if locked_package.map(|p| &p.url) == Some(&Some(url.clone()))
                        || favored_package.map(|p| &p.url) == Some(&Some(url.clone()))
                    {
                        continue;
                    }
                }
                PypiVersion::Version { version, .. } => {
                    if locked_package.map(|p| &p.version) == Some(version)
                        || favored_package.map(|p| &p.version) == Some(version)
                    {
                        continue;
                    }
                }
                PypiVersion::Extra { .. } => {
                    // Extra versions shouldn't appear in artifacts list
                    unreachable!("extras should not appear in artifact list");
                }
            }

            // Add the solvable
            let internable_version = if let PypiVersion::Version { version, .. } = artifact_version
            {
                PypiVersion::Version {
                    version: version.to_owned(),
                    package_allows_prerelease: should_package_allow_prerelease,
                }
            } else {
                artifact_version.clone()
            };

            let solvable_id = self.pool.intern_solvable(name, internable_version);
            candidates.candidates.push(solvable_id);

            // Determine the candidates
            match self.filter_artifact_candidates(artifacts) {
                Ok(artifacts) => {
                    self.cached_artifacts
                        .insert(solvable_id, artifacts.into_iter().cloned().collect());
                }
                Err(reason) => {
                    candidates
                        .excluded
                        .push((solvable_id, self.pool.intern_string(reason)));
                }
            }
        }

        // Add a locked dependency
        if let Some(locked) = self
            .options
            .locked_packages
            .get(package_name.base_package())
        {
            let version = if let Some(url) = &locked.url {
                PypiVersion::Url(url.clone())
            } else {
                PypiVersion::Version {
                    version: locked.version.clone(),
                    package_allows_prerelease: locked.version.any_prerelease(),
                }
            };
            let solvable_id = self.pool.intern_solvable(name, version);
            candidates.candidates.push(solvable_id);
            candidates.locked = Some(solvable_id);
            self.cached_artifacts
                .insert(solvable_id, locked.artifacts.clone());
        }

        // Add a favored dependency
        if let Some(favored) = self
            .options
            .favored_packages
            .get(package_name.base_package())
        {
            let version = if let Some(url) = &favored.url {
                PypiVersion::Url(url.clone())
            } else {
                PypiVersion::Version {
                    version: favored.version.clone(),
                    package_allows_prerelease: favored.version.any_prerelease(),
                }
            };
            let solvable_id = self.pool.intern_solvable(name, version);
            candidates.candidates.push(solvable_id);
            candidates.favored = Some(solvable_id);
            self.cached_artifacts
                .insert(solvable_id, favored.artifacts.clone());
        }

        Some(candidates)
    }

    async fn get_dependencies(&self, solvable_id: SolvableId) -> Dependencies {
        let solvable = self.pool.resolve_solvable(solvable_id);
        let package_name = self.pool.resolve_package_name(solvable.name);
        let package_version = &solvable.record;

        tracing::info!(
            "obtaining dependency information from {}={}",
            package_name,
            package_version
        );

        let mut dependencies = KnownDependencies::default();

        // Add a dependency to the base dependency when we have an extra
        // So that we have a connection to the base package
        if let PypiPackageName::ExtraFeature(base_package, _) = package_name {
            // Intern the base package name (creates it if it doesn't exist)
            let base_name_id = self
                .pool
                .intern_package_name(PypiPackageName::package(base_package.clone()));

            // Extra feature solvables depend on the base package with ANY version
            // The actual version constraint comes from the requirement that selected this extra
            let version_set_id = self.pool.intern_version_set(
                base_name_id,
                PypiVersionSet::from_spec(None, &self.options.pre_release_resolution),
            );
            dependencies.requirements.push(ConditionalRequirement {
                condition: None,
                requirement: ResolvoRequirement::Single(version_set_id),
            });

            // Extra features are virtual solvables with no other dependencies
            return Dependencies::Known(dependencies);
        }

        // Retrieve the artifacts that are applicable for this version
        let artifacts = self
            .cached_artifacts
            .get(&solvable_id)
            .expect("the artifacts must already have been cached");

        // If there are no artifacts we can have two cases
        if artifacts.is_empty() {
            // TODO: rework this so it makes more sense from an API perspective later, I think we should add the concept of installed_and_locked or something
            // It is locked the package data may be available externally
            // So it's fine if there are no artifacts, we can just assume this has been taken care of
            let locked_package = self
                .options
                .locked_packages
                .get(package_name.base_package());
            match package_version {
                PypiVersion::Url(url) => {
                    if locked_package.map(|p| &p.url) == Some(&Some(url.clone())) {
                        return Dependencies::Known(dependencies);
                    }
                }

                PypiVersion::Version { version, .. } => {
                    if locked_package.map(|p| &p.version) == Some(version) {
                        return Dependencies::Known(dependencies);
                    }
                }

                PypiVersion::Extra { .. } => {
                    // Extras don't have artifacts, this should have been handled earlier
                    unreachable!("extras should have been handled earlier");
                }
            }

            // Otherwise, we do expect data, and it's not fine if there are no artifacts
            let error = self.pool.intern_string(format!(
                "there are no artifacts available for {}={}",
                package_name, package_version
            ));
            return Dependencies::Unknown(error);
        }

        let result: miette::Result<_> = tokio::spawn({
            let package_db = self.package_db.clone();
            let wheel_builder = self.wheel_builder.clone();
            let artifacts = artifacts.to_vec();
            let lease = self.aquire_lease_to_run().await;
            async move {
                if let Some((ai, metadata)) = package_db
                    .get_metadata(&artifacts, Some(&wheel_builder))
                    .await?
                {
                    drop(lease);
                    Ok(Some((ai.clone(), metadata)))
                } else {
                    drop(lease);
                    Ok(None)
                }
            }
        })
        .await
        .expect("cancelled");

        let metadata = match result {
            // We have retrieved a value without error
            Ok(value) => {
                if let Some((_, metadata)) = value {
                    // Return the metadata
                    metadata
                } else {
                    let formatted_artifacts = artifacts
                        .iter()
                        .format_with("\n", |a, f| f(&format_args!("\t- {}", a.filename)))
                        .to_string();
                    // No results have been found with the methods we tried
                    *self.should_cancel_with_value.lock() =
                        Some(MetadataError::NoMetadata(formatted_artifacts));
                    return Dependencies::Unknown(self.pool.intern_string("".to_string()));
                }
            }
            // Errors have occurred during metadata extraction
            // This is almost always an sdist build failure
            Err(e) => {
                let formatted_artifacts = artifacts
                    .iter()
                    .format_with("\n", |a, f| f(&format_args!("\t- {}", a.filename)))
                    .to_string();
                *self.should_cancel_with_value.lock() = Some(MetadataError::ExtractionFailure {
                    artifacts: formatted_artifacts,
                    errors: vec![MietteDiagnostic::new(e.to_string()).with_help("Probably an error during processing of source distributions. Please check the error message above.")],
                });
                return Dependencies::Unknown(self.pool.intern_string("".to_string()));
            }
        };

        // Note: In the old implementation, we added constraints here to ensure that
        // extras matched the same version as their base package. However, with the new
        // virtual solvable model for extras, this is no longer needed or valid.
        // Extra feature solvables are virtual (they don't have real versions) and they
        // automatically depend on their base package, which provides the version constraint.
        // Therefore, we skip this constraint generation for now.
        //
        // In the future, if we need to model "provides_extras" (packages declaring which
        // extras they provide), we could potentially use this logic differently.

        let extras: Vec<ExtraName> = match package_name {
            PypiPackageName::ExtraFeature(_, extra) => {
                vec![ExtraName::new(extra.as_str().to_string()).unwrap()]
            }
            PypiPackageName::Package(_) => Vec::new(),
        };
        for requirement in metadata.requires_dist {
            // Extract marker and check if it's conditional on an extra
            let marker = &requirement.marker;

            // Check if this dependency is conditional on a specific extra
            let extra_marker = marker.top_level_extra();

            // Evaluate environment markers (but not extra markers)
            // For base packages with extra-specific dependencies, we DON'T evaluate the marker
            // because we'll add them as conditional requirements instead
            let is_extra_conditional =
                extra_marker.is_some() && matches!(package_name, PypiPackageName::Package(_));

            if !is_extra_conditional && !marker.evaluate(&self.markers, extras.as_slice()) {
                // Non-extra markers that don't match the environment
                continue;
            }

            // Add the dependency to the pool
            let Requirement {
                name,
                version_or_url,
                extras: req_extras,
                ..
            } = requirement;
            let dep_package_name =
                PackageName::from_str(name.as_ref()).expect("invalid package name");
            let dependency_name_id = self
                .pool
                .intern_package_name(PypiPackageName::package(dep_package_name.clone().into()));

            let version_set_id = self.pool.intern_version_set(
                dependency_name_id,
                PypiVersionSet::from_spec(
                    version_or_url.clone(),
                    &self.options.pre_release_resolution,
                ),
            );

            if let Some(VersionOrUrl::Url(url)) = version_or_url.clone()
                && let Some(given) = url.given()
            {
                self.name_to_url
                    .insert(dep_package_name.clone().into(), given.to_owned());
            }

            // Determine if this requirement should be conditional on an extra
            let condition = if let Some(pep508_rs::MarkerExpression::Extra {
                name: extra_value,
                ..
            }) = extra_marker
            {
                // Extract the extra name
                // and get the base package that owns this dependency
                // (package_name here refers to the outer package being processed, e.g., cachecontrol)
                if let pep508_rs::MarkerValueExtra::Extra(extra_name) = extra_value
                    && let PypiPackageName::Package(owner_pkg) = package_name
                {
                    // Convert ExtraName to Extra
                    let extra = Extra::from_str(extra_name.as_ref()).expect("invalid extra");
                    // Create a condition for this extra
                    Some(self.extra_condition(owner_pkg, &extra))
                } else {
                    None
                }
            } else {
                None
            };

            // Add requirement (conditional if it depends on an extra)
            dependencies.requirements.push(ConditionalRequirement {
                condition,
                requirement: ResolvoRequirement::Single(version_set_id),
            });

            // Add unconditional requirements for each extra feature requested
            // When a package depends on foo[bar], we require both foo and foo[bar]
            for extra_name in req_extras {
                let extra = Extra::from_str(extra_name.as_ref()).expect("invalid extra name");

                // Require the extra feature solvable (for the DEPENDENCY, not the owner)
                let extra_feature_name =
                    PypiPackageName::extra_feature(dep_package_name.clone().into(), extra);
                let extra_name_id = self.pool.intern_package_name(extra_feature_name);
                // Extra features are virtual solvables, so we use None (any version)
                // The actual version constraint is on the base package
                let extra_version_set_id = self.pool.intern_version_set(
                    extra_name_id,
                    PypiVersionSet::from_spec(None, &self.options.pre_release_resolution),
                );

                // This requirement is unconditional - we always need the extra when requested
                dependencies.requirements.push(ConditionalRequirement {
                    condition: None,
                    requirement: ResolvoRequirement::Single(extra_version_set_id),
                });
            }
        }

        Dependencies::Known(dependencies)
    }
}
